//! Per-checkout views: the family registry, v2 stores, the GC protection
//! protocol, and publication durability, across real processes.
//!
//! Child processes re-run this test binary with `--ignored --exact
//! per_checkout_registry::per_checkout_child`; the scenario and the pause
//! point come from `PER_CHECKOUT_*` environment variables. A child parks at
//! its pause point after writing a ready file, and either waits for a go file
//! or is killed by the parent.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aft::blob_store::v2::{
    ContentHash, FamilyKey, FamilyPlane, FamilyStoreReader, TrigramKey, TrigramPolicy,
};
use aft::gc::family::{sweep_family, FamilySweepPolicy, SweepObserver, SweepStep};
use aft::pins::{protect_then_touch, AssemblyPin, LivePin};
use aft::views::contracts::{DurabilityObserver, DurabilityStep};
use aft::views::manifest_v2::{
    EntryPlanes, EntryV2, GenerationName, ManifestHeader, ManifestV2, Producers, PublishV2,
};
use aft::views::readiness::PlaneState;
use aft::views::registry::{FamilyRegistry, ViewRegistration};
use aft::views::segment_store::{self, SegmentMember, SegmentReader, TrigramPayload};
use aft::views::RelPath;
use tempfile::tempdir;

const FAMILY: &str = "family";
const CHILD_TEST: &str = "per_checkout_registry::per_checkout_child";
const COLLECT_ALL: FamilySweepPolicy = FamilySweepPolicy { byte_budget: 0 };

fn policy() -> TrigramPolicy {
    TrigramPolicy {
        max_file_size: 1 << 20,
    }
}

fn producers() -> Producers {
    Producers {
        trigram: policy().fingerprint_hex(),
        semantic: None,
        callgraph: "callgraph-v1".to_string(),
    }
}

fn trigram_key(bytes: &[u8]) -> FamilyKey {
    TrigramKey {
        content: ContentHash::of(bytes),
        policy: policy(),
    }
    .family_key()
}

fn rel(value: &str) -> RelPath {
    RelPath::new(value.as_bytes().to_vec()).unwrap()
}

/// What a published generation relies on.
#[derive(Clone, Debug)]
struct Published {
    generation: String,
    keys: Vec<FamilyKey>,
    segment: [u8; 32],
}

/// Builds and publishes a generation of `files` under the protection
/// protocol: protect every key in a live pin, put (put-or-touch) each payload,
/// write the segment, write the manifest privately, then swap the pointer.
/// The live pin is released afterwards, as a fold trims it.
fn publish(
    registration: &ViewRegistration,
    files: &[(&str, &[u8])],
    observer: Option<&dyn DurabilityObserver>,
) -> Published {
    let storage = registration.registry().storage().to_path_buf();
    let store = registration.open_store(FamilyPlane::Trigram).unwrap();
    let mut live = LivePin::create(registration).unwrap();
    let keys = files
        .iter()
        .map(|(_, bytes)| trigram_key(bytes))
        .collect::<Vec<_>>();
    live.protect(&keys).unwrap();
    for (_, bytes) in files {
        store
            .put_or_touch(
                &trigram_key(bytes),
                &TrigramPayload::extract(bytes, &policy()).encode(),
            )
            .unwrap();
        if let Some(observer) = observer {
            observer.reached(DurabilityStep::BlobCommitted);
        }
    }
    let members = files
        .iter()
        .map(|(path, bytes)| SegmentMember {
            rel_path: rel(path),
            content: ContentHash::of(bytes),
            size: bytes.len() as u64,
        })
        .collect::<Vec<_>>();
    let segment = segment_store::build_from_blobs(&store, &members, &policy()).unwrap();
    live.protect_segment(&segment.id).unwrap();
    segment_store::write_segment(&store, &storage, &segment, observer).unwrap();

    let mut manifest = ManifestV2::new(ManifestHeader {
        producers: producers(),
        head_tree: None,
        ignore_fingerprint: None,
        segment: Some(aft::blob_store::v2::to_hex(&segment.id)),
    });
    for (path, bytes) in files {
        manifest
            .insert(
                rel(path),
                EntryV2::regular(
                    ContentHash::of(bytes),
                    bytes.len() as u64,
                    EntryPlanes {
                        trigram: Some(PlaneState::ready(&trigram_key(bytes))),
                        semantic: None,
                        callgraph: None,
                    },
                ),
            )
            .unwrap();
    }
    let view = registration.view_store().unwrap();
    let base = view.current_generation().unwrap();
    let name = GenerationName::for_manifest(&manifest).unwrap();
    let prepared = view
        .prepare_v2(&name, base.as_deref(), &manifest, observer)
        .unwrap();
    assert_eq!(
        view.commit_v2(prepared, observer).unwrap(),
        PublishV2::Published
    );
    registration
        .registry()
        .note_publish(registration.scope())
        .unwrap();
    drop(live);
    Published {
        generation: name.to_string(),
        keys,
        segment: segment.id,
    }
}

/// Stores a payload that nothing references once its pin is released.
fn put_garbage(registration: &ViewRegistration, bytes: &[u8]) -> FamilyKey {
    let store = registration.open_store(FamilyPlane::Trigram).unwrap();
    let key = trigram_key(bytes);
    let mut live = LivePin::create(registration).unwrap();
    live.protect(&[key]).unwrap();
    store
        .put_or_touch(&key, &TrigramPayload::extract(bytes, &policy()).encode())
        .unwrap();
    key
}

fn trigram_store(storage: &Path) -> FamilyStoreReader {
    FamilyStoreReader::open_existing(storage, FAMILY, FamilyPlane::Trigram)
        .unwrap()
        .unwrap()
}

fn assert_readable(storage: &Path, published: &Published, context: &str) {
    let store = trigram_store(storage);
    for key in &published.keys {
        assert!(
            store.get(key).unwrap().is_some(),
            "{context}: key {key} of {} is gone",
            published.generation
        );
    }
    let path = aft::blob_store::v2::segment_path(storage, FAMILY, &published.segment).unwrap();
    let reader = SegmentReader::open(&path)
        .unwrap_or_else(|error| panic!("{context}: segment of {}: {error}", published.generation));
    assert_eq!(reader.id(), published.segment);
}

// ---------------------------------------------------------------------------
// In-process protocol tests

/// A sweep requested on behalf of one view must still mark every other
/// member's generations; the old sweep marked only the calling view.
#[test]
fn a_sweep_keeps_keys_that_only_another_view_references() {
    let storage = tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let a = registry
        .register_view("scope-a", &storage.path().join("root-a"))
        .unwrap();
    let b = registry
        .register_view("scope-b", &storage.path().join("root-b"))
        .unwrap();
    let published_a = publish(&a, &[("a.txt", b"only in a")], None);
    let published_b = publish(&b, &[("b.txt", b"only in b")], None);
    let garbage = put_garbage(&a, b"garbage nobody references");

    let report = sweep_family(&registry, Some("scope-a"), COLLECT_ALL, None).unwrap();

    assert_readable(storage.path(), &published_a, "caller's view");
    assert_readable(storage.path(), &published_b, "other view");
    assert!(!trigram_store(storage.path()).contains(&garbage).unwrap());
    assert_eq!(report.deleted_blobs, 1, "{report:?}");
}

/// A third view's reader pinned on a non-current generation keeps that
/// generation's keys and segment, after the pointer moved on.
#[test]
fn a_reader_pinned_on_a_non_current_generation_keeps_it() {
    let storage = tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let b = registry
        .register_view("scope-b", &storage.path().join("root-b"))
        .unwrap();
    let old = publish(&b, &[("b.txt", b"old contents")], None);
    let reader = registry.register_reader("parent-folder").unwrap();
    let pinned = reader.protect_current("scope-b").unwrap().unwrap();
    assert_eq!(pinned.generation(), old.generation);
    let new = publish(&b, &[("b.txt", b"new contents")], None);

    sweep_family(&registry, None, COLLECT_ALL, None).unwrap();
    assert_readable(storage.path(), &old, "pinned old generation");
    assert_readable(storage.path(), &new, "current generation");

    drop(pinned);
    let report = sweep_family(&registry, None, COLLECT_ALL, None).unwrap();
    assert!(
        !trigram_store(storage.path())
            .contains(&old.keys[0])
            .unwrap(),
        "an unpinned old generation is garbage: {report:?}"
    );
    assert_eq!(report.deleted_segments, 1);
    assert_readable(storage.path(), &new, "current generation after release");
}

#[test]
fn a_malformed_pin_aborts_the_sweep_with_nothing_deleted() {
    let storage = tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let a = registry
        .register_view("scope-a", &storage.path().join("root-a"))
        .unwrap();
    publish(&a, &[("a.txt", b"kept")], None);
    let garbage = put_garbage(&a, b"would be garbage");
    fs::write(a.view_dir().join("pins").join("broken.json"), b"{not json").unwrap();

    let error = sweep_family(&registry, None, COLLECT_ALL, None).unwrap_err();
    assert!(error.to_string().contains("nothing deleted"), "{error}");
    assert!(trigram_store(storage.path()).contains(&garbage).unwrap());
}

#[cfg(unix)]
#[test]
fn an_unreadable_readers_directory_aborts_the_sweep() {
    use std::os::unix::fs::PermissionsExt as _;
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores directory permissions; nothing to prove here.
    }
    let storage = tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let a = registry
        .register_view("scope-a", &storage.path().join("root-a"))
        .unwrap();
    publish(&a, &[("a.txt", b"kept")], None);
    let garbage = put_garbage(&a, b"would be garbage");
    let readers = a.view_dir().join("readers");
    fs::create_dir_all(&readers).unwrap();
    fs::set_permissions(&readers, fs::Permissions::from_mode(0o000)).unwrap();
    let result = sweep_family(&registry, None, COLLECT_ALL, None);
    fs::set_permissions(&readers, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_err(), "{result:?}");
    assert!(trigram_store(storage.path()).contains(&garbage).unwrap());
}

/// Touching a key is refused unless a durable protection lists it first.
#[test]
fn a_touch_needs_a_durable_protection_first() {
    let storage = tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let a = registry
        .register_view("scope-a", &storage.path().join("root-a"))
        .unwrap();
    let key = put_garbage(&a, b"payload");
    let store = a.open_store(FamilyPlane::Trigram).unwrap();
    let live = LivePin::create(&a).unwrap();
    assert!(protect_then_touch(&live, &store, &[key]).is_err());
    let mut live = live;
    live.protect(&[key]).unwrap();
    let report = protect_then_touch(&live, &store, &[key]).unwrap();
    assert_eq!(report.touched, 1);
}

/// A removed root with a live marker-only reader survives two sweeps; it
/// is removed only after the reader leaves and two further sweeps run.
#[test]
fn a_removed_root_with_a_marker_only_reader_is_kept_until_two_sweeps_after_it_leaves() {
    let storage = tempdir().unwrap();
    let root = storage.path().join("root-gone");
    fs::create_dir_all(&root).unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let view = registry.register_view("scope-gone", &root).unwrap();
    let published = publish(&view, &[("gone.txt", b"protected bytes")], None);
    let view_dir = view.view_dir().to_path_buf();
    drop(view);
    age_retention_binding(&registry, &root);

    let reader = registry.register_reader("parent-folder").unwrap();
    let pinned = reader.protect_current("scope-gone").unwrap().unwrap();
    fs::remove_dir_all(&root).unwrap();

    for sweep in 1..=2 {
        let report = sweep_family(&registry, None, COLLECT_ALL, None).unwrap();
        assert!(report.deregistered.is_empty(), "sweep {sweep}: {report:?}");
        assert!(registry.member("scope-gone").unwrap().is_some());
        assert!(view_dir.is_dir());
        assert_readable(storage.path(), &published, "reader-protected generation");
    }

    drop(pinned);
    drop(reader);
    let third = sweep_family(&registry, None, COLLECT_ALL, None).unwrap();
    assert!(third.deregistered.is_empty(), "third sweep: {third:?}");
    assert!(registry.member("scope-gone").unwrap().is_some());
    assert!(view_dir.is_dir());

    let fourth = sweep_family(&registry, None, COLLECT_ALL, None).unwrap();
    assert_eq!(fourth.deregistered, vec!["scope-gone".to_string()]);
    assert!(registry.member("scope-gone").unwrap().is_none());
    assert!(!view_dir.exists());
}

/// Pauses a sweep when it reaches `step`, until the test lets it go on.
struct PauseAt {
    step: SweepStep,
    state: std::sync::Mutex<PauseState>,
    changed: std::sync::Condvar,
}

#[derive(Default)]
struct PauseState {
    reached: bool,
    released: bool,
}

impl PauseAt {
    fn new(step: SweepStep) -> Self {
        Self {
            step,
            state: std::sync::Mutex::new(PauseState::default()),
            changed: std::sync::Condvar::new(),
        }
    }

    fn wait_until_reached(&self) {
        let mut state = self.state.lock().unwrap();
        while !state.reached {
            state = self.changed.wait(state).unwrap();
        }
    }

    fn release(&self) {
        self.state.lock().unwrap().released = true;
        self.changed.notify_all();
    }
}

impl SweepObserver for PauseAt {
    fn reached(&self, step: SweepStep) {
        if step != self.step {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.reached = true;
        self.changed.notify_all();
        while !state.released {
            state = self.changed.wait(state).unwrap();
        }
    }
}

/// A live owner whose checkout root is gone pins its view after the sweep
/// found no protection, but before the sweep removed the view. The member,
/// its view directory and the new pin must all survive.
#[test]
fn an_owner_pin_taken_while_removal_is_pending_keeps_the_member() {
    let storage = tempdir().unwrap();
    let root = storage.path().join("root-gone");
    fs::create_dir_all(&root).unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let view = registry.register_view("scope-gone", &root).unwrap();
    publish(&view, &[("gone.txt", b"owner bytes")], None);
    age_retention_binding(&registry, &root);
    fs::remove_dir_all(&root).unwrap();

    // The first sweep only counts the missing root.
    let first = sweep_family(&registry, None, COLLECT_ALL, None).unwrap();
    assert!(first.deregistered.is_empty(), "{first:?}");

    // The second sweep would remove the member; the owner pins meanwhile.
    let pause = PauseAt::new(SweepStep::RemovalPending);
    let (report, pin) = thread::scope(|scope| {
        let sweeper = scope.spawn(|| sweep_family(&registry, None, COLLECT_ALL, Some(&pause)));
        pause.wait_until_reached();
        let pin = LivePin::create(&view).unwrap();
        pause.release();
        (sweeper.join().unwrap().unwrap(), pin)
    });

    assert!(report.deregistered.is_empty(), "{report:?}");
    assert!(registry.member("scope-gone").unwrap().is_some());
    assert!(pin.keys_path().is_file(), "the owner's pin was removed");
    drop(pin);
}

/// Once a missing-root member has been removed, its old registration can no
/// longer pin: creation is refused and does not re-create the view directory.
#[test]
fn a_deregistered_view_cannot_be_pinned() {
    let storage = tempdir().unwrap();
    let root = storage.path().join("root-gone");
    fs::create_dir_all(&root).unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let view = registry.register_view("scope-gone", &root).unwrap();
    age_retention_binding(&registry, &root);
    fs::remove_dir_all(&root).unwrap();
    for _ in 0..2 {
        sweep_family(&registry, None, COLLECT_ALL, None).unwrap();
    }
    assert!(registry.member("scope-gone").unwrap().is_none());

    let live = LivePin::create(&view);
    assert!(
        matches!(live, Err(aft::pins::PinError::NotRegistered(_))),
        "{live:?}"
    );
    let assembly = AssemblyPin::create_v2(&view, "late-assembly", &[]);
    assert!(
        matches!(assembly, Err(aft::pins::PinError::NotRegistered(_))),
        "{assembly:?}"
    );
    assert!(!view.view_dir().exists());
}

/// Removal race fixtures model an abandoned checkout, not a volume that went
/// missing moments after its last bind. Both durable clocks must be aged.
fn age_retention_binding(registry: &FamilyRegistry, root: &Path) {
    let connection =
        aft::db::TrackedConnection::open(registry.path(), aft::db::SqliteStore::BlobStore).unwrap();
    connection
        .execute("UPDATE members SET last_bind_ms = 0", [])
        .unwrap();
    let scope = aft::path_identity::project_scope_key(root);
    let path = registry
        .storage()
        .join(format!("retention/roots/{scope}.json"));
    let mut binding: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    binding["last_bound_ms"] = serde_json::json!(0);
    fs::write(path, serde_json::to_vec(&binding).unwrap()).unwrap();
}

/// An unreadable marker is uncertainty, and uncertainty keeps the member.
#[cfg(unix)]
#[test]
fn an_unreadable_marker_keeps_a_removed_roots_member() {
    use std::os::unix::fs::PermissionsExt as _;
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores directory permissions; nothing to prove here.
    }
    let storage = tempdir().unwrap();
    let root = storage.path().join("root-gone");
    fs::create_dir_all(&root).unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let view = registry.register_view("scope-gone", &root).unwrap();
    let published = publish(&view, &[("gone.txt", b"bytes")], None);
    let marker_dir = view.view_dir().join("readers").join(&published.generation);
    fs::create_dir_all(&marker_dir).unwrap();
    fs::write(marker_dir.join("1.host.1.0.json"), b"{}").unwrap();
    fs::set_permissions(&marker_dir, fs::Permissions::from_mode(0o000)).unwrap();
    fs::remove_dir_all(&root).unwrap();
    let mut reports = Vec::new();
    for _ in 0..3 {
        reports.push(sweep_family(&registry, None, COLLECT_ALL, None));
    }
    fs::set_permissions(&marker_dir, fs::Permissions::from_mode(0o755)).unwrap();
    for report in reports {
        let report = report.unwrap();
        assert!(report.deregistered.is_empty(), "{report:?}");
    }
    assert!(registry.member("scope-gone").unwrap().is_some());
}

/// Two builders of equal manifests write private files; one wins the CAS and
/// the other sees an equivalent winner rather than a conflict.
#[test]
fn concurrent_equal_manifest_builders_write_privately_and_agree() {
    let storage = tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let view = registry
        .register_view("scope-a", &storage.path().join("root-a"))
        .unwrap();
    let mut manifest = ManifestV2::new(ManifestHeader {
        producers: producers(),
        head_tree: None,
        ignore_fingerprint: None,
        segment: None,
    });
    manifest
        .insert(
            rel("a.txt"),
            EntryV2::regular(ContentHash::of(b"same"), 4, EntryPlanes::default()),
        )
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let outcomes = (0..2)
        .map(|_| {
            let view = view.clone();
            let manifest = manifest.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let store = view.view_store().unwrap();
                let name = GenerationName::for_manifest(&manifest).unwrap();
                let prepared = store.prepare_v2(&name, None, &manifest, None).unwrap();
                barrier.wait();
                (name.to_string(), store.commit_v2(prepared, None).unwrap())
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_ne!(outcomes[0].0, outcomes[1].0, "names must be private");
    let published = outcomes
        .iter()
        .filter(|(_, outcome)| *outcome == PublishV2::Published)
        .count();
    let equivalent = outcomes
        .iter()
        .filter(|(_, outcome)| matches!(outcome, PublishV2::EquivalentWinner { .. }))
        .count();
    assert_eq!((published, equivalent), (1, 1), "{outcomes:?}");
    for (name, _) in &outcomes {
        assert!(view
            .view_dir()
            .join(format!("manifest-{name}.json"))
            .is_file());
    }
}

// ---------------------------------------------------------------------------
// Multi-process tests

fn spawn_child(scenario: &str, env: &[(&str, &Path)], extra: &[(&str, &str)]) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env("PER_CHECKOUT_SCENARIO", scenario)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (name, value) in env {
        command.env(name, value);
    }
    for (name, value) in extra {
        command.env(name, value);
    }
    command.spawn().unwrap()
}

fn wait_for_file(child: &mut Child, path: &Path) {
    let started = Instant::now();
    while !path.is_file() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited before writing {path:?}: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "timed out waiting for {path:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "child did not exit"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn kill(child: &mut Child) {
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGKILL) }, 0);
    assert!(!child.wait().unwrap().success());
}

/// The child side of every multi-process scenario.
#[test]
#[ignore]
fn per_checkout_child() {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let storage = var("PER_CHECKOUT_STORAGE").expect("PER_CHECKOUT_STORAGE");
    let ready = var("PER_CHECKOUT_READY").expect("PER_CHECKOUT_READY");
    let scenario = std::env::var("PER_CHECKOUT_SCENARIO").unwrap();
    let registry = FamilyRegistry::open(&storage, FAMILY).unwrap();
    match scenario.as_str() {
        "sweep-pausing-after-mark" => {
            let go = var("PER_CHECKOUT_GO").expect("PER_CHECKOUT_GO");
            let done = var("PER_CHECKOUT_DONE").expect("PER_CHECKOUT_DONE");
            let observer = PauseAfterMark { ready, go };
            let report = sweep_family(&registry, None, COLLECT_ALL, Some(&observer)).unwrap();
            fs::write(
                done,
                format!("{} {}", report.deleted_blobs, report.deleted_segments),
            )
            .unwrap();
        }
        "hold-assembly-pin" => {
            let scope = std::env::var("PER_CHECKOUT_SCOPE").unwrap();
            let root = var("PER_CHECKOUT_ROOT").unwrap();
            let registration = registry.register_view(&scope, &root).unwrap();
            let key_hex = std::env::var("PER_CHECKOUT_KEY").unwrap();
            let key = FamilyKey::new(
                FamilyPlane::Trigram,
                aft::blob_store::v2::parse_hex32(&key_hex).unwrap(),
            );
            let _pin =
                AssemblyPin::create_v2(&registration, "assembly-in-progress", &[key]).unwrap();
            let store = registration.open_store(FamilyPlane::Trigram).unwrap();
            protect_then_touch(&_pin, &store, &[key]).unwrap();
            fs::write(&ready, b"ready").unwrap();
            loop {
                thread::sleep(Duration::from_millis(10));
            }
        }
        "publish" => {
            let scope = std::env::var("PER_CHECKOUT_SCOPE").unwrap();
            let root = var("PER_CHECKOUT_ROOT").unwrap();
            let step = std::env::var("PER_CHECKOUT_STEP").unwrap();
            let registration = registry.register_view(&scope, &root).unwrap();
            let files: [(&str, &[u8]); 2] = [
                ("a.txt", b"generation two a"),
                ("c.txt", b"generation two c"),
            ];
            if step == "clean-exit" {
                publish(&registration, &files, None);
                fs::write(&ready, b"published").unwrap();
                return;
            }
            let observer = ParkAt {
                step: DurabilityStep::parse(&step).unwrap(),
                ready,
            };
            publish(&registration, &files, Some(&observer));
            panic!("the pause point {step} was never reached");
        }
        other => panic!("unknown scenario {other}"),
    }
}

struct PauseAfterMark {
    ready: PathBuf,
    go: PathBuf,
}

impl SweepObserver for PauseAfterMark {
    fn reached(&self, step: SweepStep) {
        if step == SweepStep::Marked {
            fs::write(&self.ready, b"marked").unwrap();
            while !self.go.is_file() {
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

struct ParkAt {
    step: DurabilityStep,
    ready: PathBuf,
}

impl DurabilityObserver for ParkAt {
    fn reached(&self, step: DurabilityStep) {
        if step == self.step {
            fs::write(&self.ready, self.step.as_str()).unwrap();
            loop {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Runs a child sweeper that pauses after marking, performs `during` in this
/// process while it is paused, lets it finish, and returns its report text.
fn with_sweep_paused_after_marking(storage: &Path, during: impl FnOnce()) -> String {
    let control = tempdir().unwrap();
    let ready = control.path().join("ready");
    let go = control.path().join("go");
    let done = control.path().join("done");
    let mut child = spawn_child(
        "sweep-pausing-after-mark",
        &[
            ("PER_CHECKOUT_STORAGE", storage),
            ("PER_CHECKOUT_READY", &ready),
            ("PER_CHECKOUT_GO", &go),
            ("PER_CHECKOUT_DONE", &done),
        ],
        &[],
    );
    wait_for_file(&mut child, &ready);
    during();
    fs::write(&go, b"go").unwrap();
    assert!(wait_for_exit(&mut child).success(), "sweeper child failed");
    fs::read_to_string(done).unwrap()
}

/// Another process reuses an old, unreferenced key after the sweep marked.
/// The conditional delete (`ref_epoch < S`) must keep it, while a key nobody
/// reused is still collected.
#[test]
fn a_key_reused_after_marking_survives_the_sweep() {
    let storage = tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let a = registry
        .register_view("scope-a", &storage.path().join("root-a"))
        .unwrap();
    publish(&a, &[("a.txt", b"published")], None);
    let reused = put_garbage(&a, b"old payload that gets reused");
    let dropped = put_garbage(&a, b"old payload nobody reuses");

    let mut reuse_pin = None;
    let report = with_sweep_paused_after_marking(storage.path(), || {
        let store = a.open_store(FamilyPlane::Trigram).unwrap();
        let mut live = LivePin::create(&a).unwrap();
        live.protect(&[reused]).unwrap();
        let touch = protect_then_touch(&live, &store, &[reused]).unwrap();
        assert!(touch.missing.is_empty());
        reuse_pin = Some(live);
    });

    let store = trigram_store(storage.path());
    assert!(
        store.contains(&reused).unwrap(),
        "a key reused after marking was deleted (sweep report {report})"
    );
    assert!(!store.contains(&dropped).unwrap(), "report {report}");
    drop(reuse_pin);
}

/// A view that registers during a sweep, then protects, puts and publishes
/// while the sweep is paused after marking, keeps everything it published.
#[test]
fn registration_and_publication_during_a_sweep_are_safe() {
    let storage = tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let a = registry
        .register_view("scope-a", &storage.path().join("root-a"))
        .unwrap();
    publish(&a, &[("a.txt", b"a")], None);

    let mut late = None;
    with_sweep_paused_after_marking(storage.path(), || {
        let c = registry
            .register_view("scope-c", &storage.path().join("root-c"))
            .unwrap();
        late = Some(publish(
            &c,
            &[("c.txt", b"published during the sweep")],
            None,
        ));
    });
    let late = late.unwrap();
    assert_readable(storage.path(), &late, "published during the sweep");
    sweep_family(&registry, None, COLLECT_ALL, None).unwrap();
    assert_readable(storage.path(), &late, "after a later sweep");
}

/// A pin whose owner is alive but stopped past the TTL keeps its protection;
/// it is reclaimed only once the owner process is gone.
#[cfg(unix)]
#[test]
fn a_stopped_but_live_pin_owner_keeps_its_keys() {
    let storage = tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let a = registry
        .register_view("scope-a", &storage.path().join("root-a"))
        .unwrap();
    let key = put_garbage(&a, b"relied on by a stopped assembler");
    let control = tempdir().unwrap();
    let ready = control.path().join("ready");
    let root = storage.path().join("root-p");
    let mut child = spawn_child(
        "hold-assembly-pin",
        &[
            ("PER_CHECKOUT_STORAGE", storage.path()),
            ("PER_CHECKOUT_READY", &ready),
            ("PER_CHECKOUT_ROOT", &root),
        ],
        &[
            ("PER_CHECKOUT_SCOPE", "scope-p"),
            ("PER_CHECKOUT_KEY", &key.to_hex()),
        ],
    );
    wait_for_file(&mut child, &ready);
    let pin_json = registry
        .member("scope-p")
        .unwrap()
        .unwrap()
        .view_dir
        .join("pins")
        .join("assembly-in-progress.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&pin_json).unwrap()).unwrap();
    metadata["renewed_at"] = serde_json::json!(0);
    fs::write(&pin_json, serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGSTOP) }, 0);

    let report = sweep_family(&registry, None, COLLECT_ALL, None).unwrap();
    assert!(
        trigram_store(storage.path()).contains(&key).unwrap(),
        "{report:?}"
    );
    assert!(pin_json.is_file());
    assert_eq!(report.reclaimed_pins, 0, "{report:?}");

    unsafe { libc::kill(child.id() as i32, libc::SIGCONT) };
    kill(&mut child);
    let report = sweep_family(&registry, None, COLLECT_ALL, None).unwrap();
    assert!(!pin_json.exists(), "{report:?}");
    assert!(!trigram_store(storage.path()).contains(&key).unwrap());
}

/// Kill a publisher at every durability boundary. Afterwards the pointer
/// names a complete generation, every generation that is current or
/// protected stays readable through two sweeps, and the dead publisher's pins
/// are reclaimed.
#[cfg(unix)]
#[test]
fn a_publisher_killed_at_any_durability_boundary_leaves_protected_bytes_readable() {
    for step in DurabilityStep::ALL {
        let storage = tempdir().unwrap();
        let root = storage.path().join("root-a");
        fs::create_dir_all(&root).unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let view = registry.register_view("scope-a", &root).unwrap();
        let first = publish(
            &view,
            &[
                ("a.txt", b"generation one a"),
                ("b.txt", b"generation one b"),
            ],
            None,
        );
        let reader = registry.register_reader("kill-test").unwrap();
        let pinned = reader.protect_current("scope-a").unwrap().unwrap();

        let control = tempdir().unwrap();
        let ready = control.path().join("ready");
        let mut child = spawn_child(
            "publish",
            &[
                ("PER_CHECKOUT_STORAGE", storage.path()),
                ("PER_CHECKOUT_READY", &ready),
                ("PER_CHECKOUT_ROOT", &root),
            ],
            &[
                ("PER_CHECKOUT_SCOPE", "scope-a"),
                ("PER_CHECKOUT_STEP", step.as_str()),
            ],
        );
        wait_for_file(&mut child, &ready);
        kill(&mut child);

        // Restart: this process reopens everything the dead publisher touched.
        let view_store = view.view_store().unwrap();
        let current = view_store.current_generation().unwrap().unwrap();
        let after_cas = matches!(
            step,
            DurabilityStep::PointerCas | DurabilityStep::PointerSynced
        );
        assert_eq!(
            current != first.generation,
            after_cas,
            "{step:?}: pointer {current}"
        );
        let manifest = view_store.load_manifest_v2(&current).unwrap();
        for sweep in 0..2 {
            let report = sweep_family(&registry, None, COLLECT_ALL, None)
                .unwrap_or_else(|error| panic!("{step:?} sweep {sweep}: {error}"));
            if sweep == 0 {
                assert!(report.reclaimed_pins >= 1, "{step:?}: {report:?}");
            }
            assert_readable(
                storage.path(),
                &first,
                &format!("{step:?} pinned generation"),
            );
            let store = trigram_store(storage.path());
            for key in manifest.ready_keys() {
                assert!(
                    store.get(&key).unwrap().is_some(),
                    "{step:?}: current key {key}"
                );
            }
            let segment = manifest.segment_id().unwrap();
            let path = aft::blob_store::v2::segment_path(storage.path(), FAMILY, &segment).unwrap();
            assert_eq!(
                SegmentReader::open(&path).unwrap().id(),
                segment,
                "{step:?}"
            );
        }
        drop(pinned);
    }
}

/// A publisher that exits normally leaves a published generation a new
/// process reads and keeps through sweeps.
#[test]
fn a_generation_published_by_an_exited_process_survives_restart_and_sweeps() {
    let storage = tempdir().unwrap();
    let root = storage.path().join("root-a");
    fs::create_dir_all(&root).unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let control = tempdir().unwrap();
    let ready = control.path().join("ready");
    let mut child = spawn_child(
        "publish",
        &[
            ("PER_CHECKOUT_STORAGE", storage.path()),
            ("PER_CHECKOUT_READY", &ready),
            ("PER_CHECKOUT_ROOT", &root),
        ],
        &[
            ("PER_CHECKOUT_SCOPE", "scope-a"),
            ("PER_CHECKOUT_STEP", "clean-exit"),
        ],
    );
    assert!(wait_for_exit(&mut child).success());
    let member = registry.member("scope-a").unwrap().unwrap();
    let view = aft::views::registry::view_dir(storage.path(), "scope-a").unwrap();
    assert_eq!(member.view_dir, view);
    let reader = registry.register_reader("restart-check").unwrap();
    let pinned = reader.protect_current("scope-a").unwrap().unwrap();
    let registration = registry.register_view("scope-a", &root).unwrap();
    let manifest = registration
        .view_store()
        .unwrap()
        .load_manifest_v2(pinned.generation())
        .unwrap();
    drop(pinned);
    for _ in 0..2 {
        sweep_family(&registry, None, COLLECT_ALL, None).unwrap();
    }
    let store = trigram_store(storage.path());
    let keys = manifest.ready_keys().collect::<Vec<_>>();
    assert_eq!(keys.len(), 2);
    for key in keys {
        assert!(store.get(&key).unwrap().is_some());
    }
    let paths = manifest
        .entries()
        .map(|(path, _)| String::from_utf8(path.as_bytes().to_vec()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["a.txt", "c.txt"]);
}

// ---------------------------------------------------------------------------
// Parity harness

/// The membership plane of a view: a generation built from the walker plus a
/// live delta fed with the paths each edit touched (and a full membership
/// reconcile after an ignore-file change), compared after every step and
/// after a fold with an independent cold rebuild of the frozen checkout.
struct MembershipOracle {
    delta: std::cell::RefCell<aft::views::snapshot::LiveDelta>,
}

type Membership = std::collections::BTreeMap<RelPath, aft::views::snapshot::DiskState>;

fn manifest_of(membership: &Membership) -> ManifestV2 {
    let mut manifest = ManifestV2::new(ManifestHeader {
        producers: producers(),
        head_tree: None,
        ignore_fingerprint: None,
        segment: None,
    });
    for (rel_path, state) in membership {
        if let aft::views::snapshot::DiskState::Present { content, size } = state {
            manifest
                .insert(
                    rel_path.clone(),
                    EntryV2::regular(*content, *size, EntryPlanes::default()),
                )
                .unwrap();
        }
    }
    manifest
}

impl MembershipOracle {
    fn apply_paths(&self, root: &Path, paths: &[PathBuf]) {
        use aft::views::snapshot::{DiskState, LiveEntry};
        let walked = aft::views::parity_harness::walker_membership(root).unwrap();
        let mut delta = self.delta.borrow_mut();
        let reconcile_all = paths
            .iter()
            .any(|path| path == Path::new(aft::views::parity_harness::FIXTURE_IGNORE_FILE));
        let touched: Vec<RelPath> = if reconcile_all {
            let snapshot = delta.snapshot();
            let mut all = walked
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            all.extend(snapshot.membership().into_keys());
            all.into_iter().collect()
        } else {
            paths
                .iter()
                .map(|path| RelPath::from_os_path(path).unwrap())
                .collect()
        };
        for rel_path in touched {
            let state = walked.get(&rel_path).copied().unwrap_or(DiskState::Absent);
            delta.apply(rel_path, LiveEntry::new(state, 0));
        }
    }

    fn fold(&self, root: &Path) {
        use aft::views::snapshot::{carry_disk_state_only, derive_successor, OpenGeneration};
        let walked = aft::views::parity_harness::walker_membership(root).unwrap();
        let successor =
            std::sync::Arc::new(OpenGeneration::new("folded", manifest_of(&walked), None));
        let mut delta = self.delta.borrow_mut();
        let draft = derive_successor(&delta.cut(), successor, &carry_disk_state_only);
        delta
            .replay_and_swap(draft, &carry_disk_state_only)
            .unwrap();
    }
}

impl aft::views::parity_harness::PlaneOracle for MembershipOracle {
    type Observation = Membership;

    fn name(&self) -> &str {
        "membership"
    }

    fn observe_view(&self, _root: &Path) -> Membership {
        self.delta.borrow().snapshot().membership()
    }

    fn rebuild_cold(
        &self,
        frozen: &aft::views::parity_harness::FrozenCheckout,
        store: &aft::views::parity_harness::IsolatedStore,
    ) -> Membership {
        // Publish the frozen checkout into the empty store and read it back.
        let registry = FamilyRegistry::open(store.path(), FAMILY).unwrap();
        let view = registry.register_view("cold", frozen.root()).unwrap();
        let manifest =
            manifest_of(&aft::views::parity_harness::walker_membership(frozen.root()).unwrap());
        let view_store = view.view_store().unwrap();
        let name = GenerationName::for_manifest(&manifest).unwrap();
        let prepared = view_store.prepare_v2(&name, None, &manifest, None).unwrap();
        view_store.commit_v2(prepared, None).unwrap();
        let generation = view_store.current_generation().unwrap().unwrap();
        let loaded = view_store.load_manifest_v2(&generation).unwrap();
        let open = aft::views::snapshot::OpenGeneration::new(generation, loaded, None);
        aft::views::snapshot::LiveDelta::new(std::sync::Arc::new(open))
            .snapshot()
            .membership()
    }
}

#[test]
fn membership_parity_holds_through_the_standard_schedules_and_folds() {
    use aft::views::parity_harness::{check_parity, standard_schedules, FIXTURE_IGNORE_FILE};
    let base_bytes: &[u8] = b"base file\n";
    for schedule in standard_schedules("src/base.txt", base_bytes, "ignored.txt") {
        let checkout = tempdir().unwrap();
        let root = checkout.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/base.txt"), base_bytes).unwrap();
        fs::write(root.join("src/other.txt"), b"other\n").unwrap();
        fs::write(root.join(FIXTURE_IGNORE_FILE), b"ignored.txt\n").unwrap();
        let initial = aft::views::parity_harness::walker_membership(root).unwrap();
        let oracle = MembershipOracle {
            delta: std::cell::RefCell::new(aft::views::snapshot::LiveDelta::new(
                std::sync::Arc::new(aft::views::snapshot::OpenGeneration::new(
                    "initial",
                    manifest_of(&initial),
                    None,
                )),
            )),
        };
        for (index, step) in schedule.steps.iter().enumerate() {
            step.apply(root).unwrap();
            oracle.apply_paths(root, &step.paths());
            let scratch = tempdir().unwrap();
            let context = format!("{} step {index} ({step:?})", schedule.name);
            if let Err(mismatch) = check_parity(&oracle, root, scratch.path(), &context).unwrap() {
                panic!("{mismatch}");
            }
        }
        oracle.fold(root);
        let scratch = tempdir().unwrap();
        let context = format!("{} after fold", schedule.name);
        if let Err(mismatch) = check_parity(&oracle, root, scratch.path(), &context).unwrap() {
            panic!("{mismatch}");
        }
    }
}
