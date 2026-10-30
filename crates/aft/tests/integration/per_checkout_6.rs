//! Per-checkout views: disk limits and least-recently-used eviction across
//! real processes. A session held by another live process blocks eviction
//! until that process dies; a process killed in the middle of an eviction or
//! a segment deletion leaves protected data readable and a state the next
//! pass finishes.
//!
//! Child processes re-run this test binary with `--ignored --exact
//! per_checkout_6::per_checkout_6_child`; the scenario and the pause point
//! come from `PC6_*` environment variables. A child parks at its pause point
//! after writing a ready file, and either waits for a go file or is killed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aft::blob_store::v2::{
    ContentHash, FamilyKey, FamilyPlane, FamilyStoreReader, TrigramKey, TrigramPolicy,
};
use aft::gc::family::{sweep_family, FamilySweepPolicy, SweepObserver, SweepStep};
use aft::pins::LivePin;
use aft::views::eviction::{
    enforce, DiskBudget, EnforceOptions, EvictionObserver, EvictionStep, Hold,
};
use aft::views::manifest_v2::{
    EntryPlanes, EntryV2, GenerationName, ManifestHeader, ManifestV2, Producers, PublishV2,
};
use aft::views::readiness::PlaneState;
use aft::views::registry::{FamilyRegistry, ViewRegistration};
use aft::views::segment_store::{self, SegmentMember, SegmentReader, TrigramPayload};
use aft::views::RelPath;
use tempfile::tempdir;

const FAMILY: &str = "family";
const CHILD_TEST: &str = "per_checkout_6::per_checkout_6_child";

fn policy() -> TrigramPolicy {
    TrigramPolicy {
        max_file_size: 1 << 20,
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

/// Everything one published generation relies on in the family store.
#[derive(Clone, Debug)]
struct Published {
    generation: String,
    keys: Vec<FamilyKey>,
    segment: [u8; 32],
}

/// Publishes `files` as the view's next generation under the protection
/// protocol, and writes a small database as its derived file.
fn publish(view: &ViewRegistration, files: &[(&str, &[u8])]) -> Published {
    let storage = view.registry().storage().to_path_buf();
    let store = view.open_store(FamilyPlane::Trigram).unwrap();
    let mut live = LivePin::create(view).unwrap();
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
    segment_store::write_segment(&store, &storage, &segment, None).unwrap();
    let mut manifest = ManifestV2::new(ManifestHeader {
        producers: Producers {
            trigram: policy().fingerprint_hex(),
            semantic: None,
            callgraph: "callgraph-v1".to_string(),
        },
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
    let view_store = view.view_store().unwrap();
    let base = view_store.current_generation().unwrap();
    let name = GenerationName::for_manifest(&manifest).unwrap();
    let prepared = view_store
        .prepare_v2(&name, base.as_deref(), &manifest, None)
        .unwrap();
    assert_eq!(
        view_store.commit_v2(prepared, None).unwrap(),
        PublishV2::Published
    );
    drop(live);
    let generation = name.to_string();
    let derived = view_store.derived_path(&generation).unwrap();
    rusqlite::Connection::open(&derived)
        .unwrap()
        .execute_batch(
            "CREATE TABLE pad (value BLOB NOT NULL);
             INSERT INTO pad VALUES (zeroblob(65536));",
        )
        .unwrap();
    Published {
        generation,
        keys,
        segment: segment.id,
    }
}

fn assert_readable(storage: &Path, published: &Published, context: &str) {
    let store = FamilyStoreReader::open_existing(storage, FAMILY, FamilyPlane::Trigram)
        .unwrap()
        .unwrap();
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

fn small_budget() -> DiskBudget {
    DiskBudget {
        view_store_soft: 0,
        view_store_hard: u64::MAX,
        ..DiskBudget::default()
    }
}

fn evicted(report: &aft::views::eviction::EnforcementReport) -> Vec<&str> {
    report
        .evicted
        .iter()
        .map(|(scope, _)| scope.as_str())
        .collect()
}

fn root_of(storage: &Path, scope: &str) -> PathBuf {
    let root = storage.join("roots").join(scope);
    fs::create_dir_all(&root).unwrap();
    root
}

// ---------------------------------------------------------------------------
// Child process plumbing

fn spawn_child(scenario: &str, storage: &Path, extra: &[(&str, &str)]) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env("PC6_SCENARIO", scenario)
        .env("PC6_STORAGE", storage)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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

fn kill(child: &mut Child) {
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
}

/// Writes the ready file, then waits for the go file, which a parent that
/// kills the child never writes.
fn park(storage: &Path) {
    fs::write(storage.join("ready"), b"ready").unwrap();
    let go = storage.join("go");
    while !go.is_file() {
        thread::sleep(Duration::from_millis(10));
    }
}

struct ParkAtEviction(EvictionStep, PathBuf);

impl EvictionObserver for ParkAtEviction {
    fn reached(&self, step: EvictionStep) {
        if step == self.0 {
            park(&self.1);
        }
    }
}

struct ParkAtSweep(SweepStep, PathBuf);

impl SweepObserver for ParkAtSweep {
    fn reached(&self, step: SweepStep) {
        if step == self.0 {
            park(&self.1);
        }
    }
}

#[test]
#[ignore = "child process of the per_checkout_6 tests"]
fn per_checkout_6_child() {
    let Ok(scenario) = std::env::var("PC6_SCENARIO") else {
        return;
    };
    let storage = PathBuf::from(std::env::var("PC6_STORAGE").unwrap());
    let registry = FamilyRegistry::open(&storage, FAMILY).unwrap();
    // A production process binds its durable owner before it may remove a
    // view directory; this child stands in for one.
    registry.bind_durable_storage_for_tests(&storage).unwrap();
    match scenario.as_str() {
        // Holds a session on `held` until killed.
        "hold" => {
            let view = registry
                .register_view_with_storage(registry.storage(), "held", &root_of(&storage, "held"))
                .unwrap();
            publish(&view, &[("held.txt", b"held by a live process\n")]);
            park(&storage);
            drop(view);
        }
        // Re-binds `held` after its eviction, publishes, and exits normally.
        "rebind" => {
            let view = registry
                .register_view_with_storage(registry.storage(), "held", &root_of(&storage, "held"))
                .unwrap();
            publish(&view, &[("held.txt", b"held again after a restart\n")]);
        }
        "evict" => {
            let step = match std::env::var("PC6_STEP").unwrap().as_str() {
                "begun" => EvictionStep::Begun,
                "pointer_removed" => EvictionStep::PointerRemoved,
                other => panic!("unknown step {other}"),
            };
            registry.set_disk_budget(small_budget());
            let observer = ParkAtEviction(step, storage.clone());
            enforce(
                &registry,
                EnforceOptions {
                    deadline: None,
                    observer: Some(&observer),
                },
            )
            .unwrap();
        }
        "sweep" => {
            let step = match std::env::var("PC6_STEP").unwrap().as_str() {
                "row_deleted" => SweepStep::SegmentRowDeleted,
                "file_unlinked" => SweepStep::SegmentFileUnlinked,
                other => panic!("unknown step {other}"),
            };
            let observer = ParkAtSweep(step, storage.clone());
            sweep_family(
                &registry,
                None,
                FamilySweepPolicy { byte_budget: 0 },
                Some(&observer),
            )
            .unwrap();
        }
        other => panic!("unknown scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// Tests

/// A session is a property of a live process. While another process holds
/// one, its view is never evicted; once that process is killed, the view is
/// evicted on the next pass; a restarted process re-binds it as a new view,
/// and a process that exits normally leaves no session behind.
#[test]
fn another_processes_session_blocks_eviction_until_it_dies_and_a_restart_rebinds() {
    let storage = tempdir().unwrap();
    let mut holder = spawn_child("hold", storage.path(), &[]);
    wait_for_file(&mut holder, &storage.path().join("ready"));
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let idle = registry
        .register_view_with_storage(registry.storage(), "idle", &root_of(storage.path(), "idle"))
        .unwrap();
    publish(&idle, &[("idle.txt", b"nobody uses this\n")]);
    drop(idle);
    registry.set_disk_budget(small_budget());

    let report = enforce(&registry, EnforceOptions::default()).unwrap();

    assert_eq!(evicted(&report), ["idle"]);
    assert!(report
        .held
        .contains(&("held".to_string(), Hold::ActiveSession)));
    let held_dir = registry.member("held").unwrap().unwrap().view_dir;
    assert!(held_dir.is_dir());

    kill(&mut holder);
    let report = enforce(&registry, EnforceOptions::default()).unwrap();
    assert_eq!(evicted(&report), ["held"], "{:?}", report.held);
    assert!(!held_dir.exists());
    assert!(registry.member("held").unwrap().is_none());

    fs::remove_file(storage.path().join("ready")).unwrap();
    let mut restarted = spawn_child("rebind", storage.path(), &[]);
    assert!(wait_for_exit(&mut restarted).success());
    let member = registry.member("held").unwrap().expect("re-bound");
    assert!(member.view_dir.join("pointer.sqlite").is_file());
    assert!(
        registry
            .sessions()
            .unwrap()
            .iter()
            .all(|session| session.scope != "held"),
        "a process that exits normally ends its session"
    );
    let report = enforce(&registry, EnforceOptions::default()).unwrap();
    assert_eq!(evicted(&report), ["held"]);
}

/// Killed after the eviction is marked and again after the pointer is
/// removed: the view held by a session stays readable, and the next pass
/// finishes the interrupted eviction.
#[test]
fn a_kill_during_eviction_keeps_held_data_and_the_next_pass_finishes() {
    for step in ["begun", "pointer_removed"] {
        let storage = tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let kept = registry
            .register_view_with_storage(
                registry.storage(),
                "kept",
                &root_of(storage.path(), "kept"),
            )
            .unwrap();
        let kept_generation = publish(&kept, &[("kept.txt", b"kept by a session\n")]);
        let victim = registry
            .register_view_with_storage(
                registry.storage(),
                "victim",
                &root_of(storage.path(), "victim"),
            )
            .unwrap();
        publish(&victim, &[("victim.txt", b"evicted\n")]);
        let victim_dir = victim.view_dir().to_path_buf();
        drop(victim);

        let mut child = spawn_child("evict", storage.path(), &[("PC6_STEP", step)]);
        wait_for_file(&mut child, &storage.path().join("ready"));
        kill(&mut child);

        let member = registry
            .member("victim")
            .unwrap()
            .expect("still registered");
        assert!(member.evicting, "{step}: the begun state is durable");
        assert_eq!(
            victim_dir.join("pointer.sqlite").is_file(),
            step == "begun",
            "{step}: the pointer goes first"
        );
        assert_readable(storage.path(), &kept_generation, step);
        let kept_store = kept.view_store().unwrap();
        assert_eq!(
            kept_store.current_generation().unwrap().as_deref(),
            Some(kept_generation.generation.as_str())
        );

        registry.set_disk_budget(DiskBudget {
            view_store_soft: u64::MAX,
            ..small_budget()
        });
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        assert_eq!(
            evicted(&report),
            ["victim"],
            "{step}: an interrupted eviction is finished even under the limit"
        );
        assert!(!victim_dir.exists());
        assert_readable(storage.path(), &kept_generation, step);
        drop(kept);
    }
}

/// Writes the segment of `files` again, as a builder adopting it would: a
/// live pin protects the keys and the segment first, the payloads are put
/// (or touched), then the segment is written. The pin is returned so the
/// caller decides how long the segment stays protected.
fn recreate_segment(view: &ViewRegistration, files: &[(&str, &[u8])]) -> ([u8; 32], LivePin) {
    let storage = view.registry().storage().to_path_buf();
    let store = view.open_store(FamilyPlane::Trigram).unwrap();
    let mut live = LivePin::create(view).unwrap();
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
    segment_store::write_segment(&store, &storage, &segment, None).unwrap();
    (segment.id, live)
}

fn assert_segment_valid(storage: &Path, segment: &[u8; 32], context: &str) {
    let path = aft::blob_store::v2::segment_path(storage, FAMILY, segment).unwrap();
    let reader = SegmentReader::open(&path)
        .unwrap_or_else(|error| panic!("{context}: segment file: {error}"));
    assert_eq!(&reader.id(), segment, "{context}");
}

/// A sweep killed between deleting a segment's row and committing, and
/// again after unlinking its file, rolls the row back. Protected data stays
/// readable; recreating the segment writes a valid file whether or not the
/// killed sweep had unlinked it; a sweep keeps it while it is protected and
/// removes it, row and file, once it is not.
#[test]
fn a_kill_during_segment_deletion_then_recreation_finishes_safely() {
    let old_files: &[(&str, &[u8])] = &[("old.txt", b"content the view moved away from\n")];
    for step in ["row_deleted", "file_unlinked"] {
        let storage = tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let view = registry
            .register_view_with_storage(
                registry.storage(),
                "view",
                &root_of(storage.path(), "view"),
            )
            .unwrap();
        let old = publish(&view, old_files);
        let current = publish(&view, &[("new.txt", b"current content\n")]);
        let old_segment =
            aft::blob_store::v2::segment_path(storage.path(), FAMILY, &old.segment).unwrap();

        let mut child = spawn_child("sweep", storage.path(), &[("PC6_STEP", step)]);
        wait_for_file(&mut child, &storage.path().join("ready"));
        kill(&mut child);

        assert_readable(storage.path(), &current, step);
        assert_eq!(
            old_segment.is_file(),
            step == "row_deleted",
            "{step}: the file is unlinked only after the row"
        );
        let store = view.open_store(FamilyPlane::Trigram).unwrap();
        assert!(
            store.segment(&old.segment).unwrap().is_some(),
            "{step}: the uncommitted row deletion rolled back"
        );

        let (recreated, pin) = recreate_segment(&view, old_files);
        assert_eq!(recreated, old.segment);
        assert_segment_valid(storage.path(), &recreated, step);
        let report =
            sweep_family(&registry, None, FamilySweepPolicy { byte_budget: 0 }, None).unwrap();
        assert_eq!(
            report.deleted_segments, 0,
            "{step}: the pinned segment stays"
        );
        assert_segment_valid(storage.path(), &recreated, step);
        assert_readable(storage.path(), &current, step);

        drop(pin);
        let report =
            sweep_family(&registry, None, FamilySweepPolicy { byte_budget: 0 }, None).unwrap();
        assert_eq!(report.deleted_segments, 1, "{step}: then it is collected");
        assert!(!old_segment.exists());
        assert!(store.segment(&old.segment).unwrap().is_none());
        assert_readable(storage.path(), &current, step);
        drop(view);
    }
}

/// The deleting process holds the store's write lock from the row delete
/// to the commit, so a second process that re-adopts the same segment
/// meanwhile waits, then writes the file again after the unlink; the unlink
/// never removes the file it adopted.
#[test]
fn a_segment_adopted_while_another_process_deletes_it_survives() {
    let old_files: &[(&str, &[u8])] = &[("old.txt", b"content adopted again\n")];
    let storage = tempdir().unwrap();
    let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
    let view = registry
        .register_view_with_storage(registry.storage(), "view", &root_of(storage.path(), "view"))
        .unwrap();
    let old = publish(&view, old_files);
    publish(&view, &[("new.txt", b"current content\n")]);

    let mut child = spawn_child("sweep", storage.path(), &[("PC6_STEP", "row_deleted")]);
    wait_for_file(&mut child, &storage.path().join("ready"));
    let adopter = thread::spawn({
        let view = view.clone();
        move || {
            let (segment, pin) = recreate_segment(&view, old_files);
            drop(pin);
            segment
        }
    });
    thread::sleep(Duration::from_millis(300));
    assert!(
        !adopter.is_finished(),
        "the adopter waits for the deleter's lock"
    );
    fs::write(storage.path().join("go"), b"go").unwrap();
    assert!(wait_for_exit(&mut child).success());
    let adopted = adopter.join().unwrap();

    assert_eq!(adopted, old.segment);
    assert_segment_valid(storage.path(), &adopted, "after the handoff");
    let store = view.open_store(FamilyPlane::Trigram).unwrap();
    assert!(store.segment(&adopted).unwrap().is_some());
    drop(view);
}
