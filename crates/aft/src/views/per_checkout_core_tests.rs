//! Tests of the per-checkout snapshot and readiness contracts on in-memory
//! fixtures. The generation-switch tests compare every snapshot with a model of
//! the disk; the readiness tests follow pending work through folds, switches
//! and a manifest reopen.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::blob_store::v2::{ContentHash, FamilyKey, FamilyPlane};

use super::manifest_v2::{
    EntryPlanes, EntryV2, GenerationName, ManifestHeader, ManifestV2, Producers,
};
use super::readiness::{
    fold, plane_readiness, work_queue, Admission, Completion, FillMap, PlaneReadiness, PlaneState,
    WorkItem,
};
use super::snapshot::{
    carry_disk_state_only, derive_successor, DiskState, LiveDelta, LiveEntry, OpenGeneration,
    Snapshot, SwitchError,
};
use super::RelPath;

type Disk = BTreeMap<RelPath, Vec<u8>>;

fn path(value: &str) -> RelPath {
    RelPath::new(value.as_bytes().to_vec()).unwrap()
}

fn producers() -> Producers {
    Producers {
        trigram: "trigram-policy".to_string(),
        semantic: Some("model-a".to_string()),
        callgraph: "callgraph-v1".to_string(),
    }
}

fn header() -> ManifestHeader {
    ManifestHeader {
        producers: producers(),
        head_tree: None,
        ignore_fingerprint: None,
        segment: None,
    }
}

fn entry(bytes: &[u8], semantic: PlaneState) -> EntryV2 {
    EntryV2::regular(
        ContentHash::of(bytes),
        bytes.len() as u64,
        EntryPlanes {
            trigram: None,
            semantic: Some(semantic),
            callgraph: None,
        },
    )
}

/// A generation as a fold that read `disk` would publish it.
fn generation_of(disk: &Disk, name: &str) -> Arc<OpenGeneration> {
    let mut manifest = ManifestV2::new(header());
    for (rel_path, bytes) in disk {
        manifest
            .insert(
                rel_path.clone(),
                entry(bytes, PlaneState::pending("queued")),
            )
            .unwrap();
    }
    Arc::new(OpenGeneration::new(name, manifest, None))
}

fn observe(delta: &mut LiveDelta, rel_path: &RelPath, disk: &Disk) {
    let state = disk
        .get(rel_path)
        .map_or(DiskState::Absent, |bytes| DiskState::of_bytes(bytes));
    delta.apply(rel_path.clone(), LiveEntry::new(state, 0));
}

fn write(disk: &mut Disk, delta: &mut LiveDelta, rel_path: &str, bytes: &[u8]) {
    let rel_path = path(rel_path);
    disk.insert(rel_path.clone(), bytes.to_vec());
    observe(delta, &rel_path, disk);
}

fn delete(disk: &mut Disk, delta: &mut LiveDelta, rel_path: &str) {
    let rel_path = path(rel_path);
    disk.remove(&rel_path);
    observe(delta, &rel_path, disk);
}

/// Every path the snapshot or the disk knows must agree with the disk, and a
/// live entry must exist exactly where disk differs from the generation.
fn assert_matches_disk(snapshot: &Snapshot, disk: &Disk, context: &str) {
    let mut paths = disk.keys().cloned().collect::<BTreeSet<_>>();
    paths.extend(
        snapshot
            .generation()
            .flatten()
            .unwrap()
            .entries()
            .map(|(rel_path, _)| rel_path.clone()),
    );
    paths.extend(
        snapshot
            .live_entries()
            .map(|(rel_path, _)| rel_path.clone()),
    );
    for rel_path in paths {
        let expected = disk
            .get(&rel_path)
            .map_or(DiskState::Absent, |bytes| DiskState::of_bytes(bytes));
        assert_eq!(
            snapshot.disk_state(&rel_path),
            expected,
            "{context}: {:?}",
            String::from_utf8_lossy(rel_path.as_bytes())
        );
        let has_entry = snapshot
            .live_entries()
            .any(|(live_path, _)| live_path == &rel_path);
        assert_eq!(
            has_entry,
            expected != snapshot.generation().disk_state(&rel_path),
            "{context}: live entry must exist exactly where disk differs from the generation for {:?}",
            String::from_utf8_lossy(rel_path.as_bytes())
        );
    }
    let membership = snapshot.membership();
    let expected_membership = disk
        .iter()
        .map(|(rel_path, bytes)| (rel_path.clone(), DiskState::of_bytes(bytes)))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(membership, expected_membership, "{context}: membership");
}

/// The point, relative to a fold, at which an event lands.
#[derive(Clone, Copy, Debug)]
enum Point {
    BeforeFoldRead,
    AfterFoldRead,
    AfterCut,
    AfterDerive,
}

const POINTS: [Point; 4] = [
    Point::BeforeFoldRead,
    Point::AfterFoldRead,
    Point::AfterCut,
    Point::AfterDerive,
];

/// Runs one fold of `delta` while `event` lands at `point`, then swaps and
/// checks the snapshot against the disk model.
fn fold_with_event(
    disk: &mut Disk,
    delta: &mut LiveDelta,
    point: Point,
    event: &dyn Fn(&mut Disk, &mut LiveDelta),
    context: &str,
) {
    if matches!(point, Point::BeforeFoldRead) {
        event(disk, delta);
    }
    let successor = generation_of(disk, "successor");
    if matches!(point, Point::AfterFoldRead) {
        event(disk, delta);
    }
    let cut = delta.cut();
    if matches!(point, Point::AfterCut) {
        event(disk, delta);
    }
    let draft = derive_successor(&cut, successor, &carry_disk_state_only);
    if matches!(point, Point::AfterDerive) {
        event(disk, delta);
    }
    delta
        .replay_and_swap(draft, &carry_disk_state_only)
        .unwrap();
    assert_matches_disk(&delta.snapshot(), disk, context);
}

fn base_disk() -> Disk {
    let mut disk = Disk::new();
    disk.insert(path("a.txt"), b"alpha".to_vec());
    disk.insert(path("b.txt"), b"beta".to_vec());
    disk
}

/// The fold reads an edit, then the edit is reverted to G's bytes before the
/// swap. Deriving over the live keys alone would lose the revert, because the
/// revert removed the live entry.
#[test]
fn switch_keeps_a_revert_to_the_old_bytes_made_after_the_fold_read() {
    for point in POINTS {
        let mut disk = base_disk();
        let mut delta = LiveDelta::new(generation_of(&disk, "g"));
        write(&mut disk, &mut delta, "a.txt", b"edited");
        fold_with_event(
            &mut disk,
            &mut delta,
            point,
            &|disk, delta| write(disk, delta, "a.txt", b"alpha"),
            &format!("revert at {point:?}"),
        );
    }
}

#[test]
fn switch_keeps_an_add_then_delete_made_after_the_fold_read() {
    for point in POINTS {
        let mut disk = base_disk();
        let mut delta = LiveDelta::new(generation_of(&disk, "g"));
        write(&mut disk, &mut delta, "new.txt", b"transient");
        fold_with_event(
            &mut disk,
            &mut delta,
            point,
            &|disk, delta| delete(disk, delta, "new.txt"),
            &format!("add then delete at {point:?}"),
        );
    }
}

#[test]
fn switch_keeps_a_delete_then_recreate_and_an_a_b_a_switch() {
    for point in POINTS {
        let mut disk = base_disk();
        let mut delta = LiveDelta::new(generation_of(&disk, "g"));
        delete(&mut disk, &mut delta, "b.txt");
        fold_with_event(
            &mut disk,
            &mut delta,
            point,
            &|disk, delta| write(disk, delta, "b.txt", b"recreated"),
            &format!("delete then recreate at {point:?}"),
        );

        let mut disk = base_disk();
        let mut delta = LiveDelta::new(generation_of(&disk, "g"));
        write(&mut disk, &mut delta, "a.txt", b"branch b");
        write(&mut disk, &mut delta, "only-b.txt", b"b");
        fold_with_event(
            &mut disk,
            &mut delta,
            point,
            &|disk, delta| {
                write(disk, delta, "a.txt", b"alpha");
                delete(disk, delta, "only-b.txt");
            },
            &format!("A to B to A at {point:?}"),
        );
    }
}

/// Another daemon's generation wins the CAS. It read disk at a different time
/// and contains paths this daemon never saw change; the switch still ends
/// equal to disk.
#[test]
fn switch_against_a_foreign_winner_matches_disk() {
    let mut disk = base_disk();
    let mut delta = LiveDelta::new(generation_of(&disk, "g"));
    write(&mut disk, &mut delta, "a.txt", b"ours");
    let mut foreign_view = disk.clone();
    foreign_view.insert(path("a.txt"), b"older foreign read".to_vec());
    foreign_view.insert(path("c.txt"), b"foreign only".to_vec());
    foreign_view.remove(&path("b.txt"));
    let foreign = generation_of(&foreign_view, "foreign");
    let draft = derive_successor(&delta.cut(), foreign, &carry_disk_state_only);
    delta
        .replay_and_swap(draft, &carry_disk_state_only)
        .unwrap();
    assert_matches_disk(&delta.snapshot(), &disk, "foreign winner");
}

#[test]
fn switch_discards_a_successor_built_before_a_reconcile() {
    let mut disk = base_disk();
    let mut delta = LiveDelta::new(generation_of(&disk, "g"));
    write(&mut disk, &mut delta, "a.txt", b"edited");
    let cut = delta.cut();
    delta.bump_epoch();
    let draft = derive_successor(
        &cut,
        generation_of(&disk, "successor"),
        &carry_disk_state_only,
    );
    assert_eq!(
        delta.replay_and_swap(draft, &carry_disk_state_only),
        Err(SwitchError::EpochChanged { cut: 0, current: 1 })
    );
}

#[test]
fn a_snapshot_taken_before_the_swap_keeps_its_generation_and_delta() {
    let mut disk = base_disk();
    let mut delta = LiveDelta::new(generation_of(&disk, "g"));
    write(&mut disk, &mut delta, "a.txt", b"edited");
    let before = delta.snapshot();
    let before_disk = disk.clone();
    let draft = derive_successor(
        &delta.cut(),
        generation_of(&disk, "g1"),
        &carry_disk_state_only,
    );
    delta
        .replay_and_swap(draft, &carry_disk_state_only)
        .unwrap();
    write(&mut disk, &mut delta, "b.txt", b"later");
    assert_eq!(before.generation().name(), "g");
    assert_matches_disk(&before, &before_disk, "old snapshot");
    assert_eq!(delta.snapshot().generation().name(), "g1");
    assert_matches_disk(&delta.snapshot(), &disk, "new snapshot");
}

#[test]
fn intent_is_visible_to_snapshots_until_resolved() {
    let disk = base_disk();
    let mut delta = LiveDelta::new(generation_of(&disk, "g"));
    let version = delta.record_intent(path("a.txt"));
    let snapshot = delta.snapshot();
    assert_eq!(
        snapshot.pending_intent().cloned().collect::<Vec<_>>(),
        vec![path("a.txt")]
    );
    delta.resolve_intent(&path("a.txt"), version);
    assert_eq!(delta.snapshot().pending_intent().count(), 0);
    assert_eq!(
        snapshot.pending_intent().count(),
        1,
        "old snapshot keeps its intent"
    );
}

fn semantic_key(rel_path: &str, bytes: &[u8], producer: &str) -> FamilyKey {
    let key = crate::blob_store::SemanticKey::for_current(bytes, rel_path.as_bytes(), producer)
        .full_key();
    FamilyKey::from(&key)
}

fn applies(_: &RelPath) -> bool {
    true
}

/// A fold publishes new content before its semantic fill completes. After the
/// switch disk equals the new generation, so there is no live entry left; the
/// pending work must still be counted, from the generation's entry.
#[test]
fn pending_work_survives_a_fold_that_publishes_before_its_fill() {
    let mut disk = base_disk();
    let mut ready = ManifestV2::new(header());
    for (rel_path, bytes) in &disk {
        let name = String::from_utf8(rel_path.as_bytes().to_vec()).unwrap();
        ready
            .insert(
                rel_path.clone(),
                entry(
                    bytes,
                    PlaneState::ready(&semantic_key(&name, bytes, "model-a")),
                ),
            )
            .unwrap();
    }
    let base = Arc::new(OpenGeneration::new("g", ready.clone(), None));
    let mut delta = LiveDelta::new(base);
    write(&mut disk, &mut delta, "a.txt", b"edited");
    let fill = FillMap::default();
    assert_eq!(
        plane_readiness(
            FamilyPlane::Semantic,
            Some(delta.base()),
            &delta,
            &fill,
            &applies
        ),
        PlaneReadiness::Ready {
            pending: 1,
            failed: 0
        }
    );

    let folded = fold(
        delta.base(),
        &producers(),
        [(
            path("a.txt"),
            Some(entry(b"edited", PlaneState::pending("queued"))),
        )],
        &fill,
    )
    .unwrap();
    let successor = Arc::new(OpenGeneration::new("g1", folded.clone(), None));
    let draft = derive_successor(&delta.cut(), successor, &carry_disk_state_only);
    delta
        .replay_and_swap(draft, &carry_disk_state_only)
        .unwrap();
    assert_eq!(delta.snapshot().live_entries().count(), 0);
    assert_eq!(
        plane_readiness(
            FamilyPlane::Semantic,
            Some(delta.base()),
            &delta,
            &fill,
            &applies
        ),
        PlaneReadiness::Ready {
            pending: 1,
            failed: 0
        },
        "pending work must survive the fold"
    );

    let queue = work_queue(
        FamilyPlane::Semantic,
        "model-a",
        Some(delta.base()),
        &delta,
        &fill,
        &applies,
    );
    assert_eq!(
        queue,
        vec![WorkItem {
            plane: FamilyPlane::Semantic,
            rel_path: path("a.txt"),
            content: ContentHash::of(b"edited"),
            producer: "model-a".to_string(),
        }]
    );
    let mut fill = fill;
    let completion = Completion {
        item: queue[0].clone(),
        key: semantic_key("a.txt", b"edited", "model-a"),
    };
    assert_eq!(
        fill.admit(&completion, "model-a", None, Some(delta.base())),
        Admission::Installed
    );
    assert_eq!(
        plane_readiness(
            FamilyPlane::Semantic,
            Some(delta.base()),
            &delta,
            &fill,
            &applies
        ),
        PlaneReadiness::Ready {
            pending: 0,
            failed: 0
        }
    );
    let filled = fold(delta.base(), &producers(), [], &fill).unwrap();
    assert_eq!(
        filled
            .get(&path("a.txt"))
            .unwrap()
            .plane_state(FamilyPlane::Semantic),
        Some(&PlaneState::ready(&completion.key))
    );
    fill.trim_folded(&OpenGeneration::new("filled", filled, None));
    assert!(fill.is_empty());
}

/// Pending and failed states are part of the published manifest, so they
/// survive writing, publishing and reopening it.
#[test]
fn pending_and_failed_states_survive_publication_and_reopen() {
    let storage = tempfile::tempdir().unwrap();
    let registry = super::registry::FamilyRegistry::open(storage.path(), "family").unwrap();
    let registration = registry
        .register_view_with_storage(registry.storage(), "scope", &storage.path().join("root"))
        .unwrap();
    let mut manifest = ManifestV2::new(header());
    manifest
        .insert(
            path("a.txt"),
            entry(b"alpha", PlaneState::pending("queued")),
        )
        .unwrap();
    manifest
        .insert(
            path("b.txt"),
            entry(
                b"beta",
                PlaneState::Failed {
                    reason: "embed failed".to_string(),
                    producer: "model-a".to_string(),
                },
            ),
        )
        .unwrap();
    let store = registration.view_store().unwrap();
    let name = GenerationName::for_manifest(&manifest).unwrap();
    let prepared = store.prepare_v2(&name, None, &manifest, None).unwrap();
    store.commit_v2(prepared, None).unwrap();

    let reopened = super::ViewStore::existing_dir(registration.view_dir().to_path_buf()).unwrap();
    let current = reopened.current_generation().unwrap().unwrap();
    let loaded = reopened.load_manifest_v2(&current).unwrap();
    let base = Arc::new(OpenGeneration::new(current, loaded.clone(), None));
    let delta = LiveDelta::new(base);
    assert_eq!(
        plane_readiness(
            FamilyPlane::Semantic,
            Some(delta.base()),
            &delta,
            &FillMap::default(),
            &applies
        ),
        PlaneReadiness::Ready {
            pending: 1,
            failed: 1
        }
    );
}

#[test]
fn stale_completions_are_dropped() {
    let mut disk = base_disk();
    let generation = generation_of(&disk, "g");
    let manifest = generation.clone();
    let mut delta = LiveDelta::new(generation);
    let item = WorkItem {
        plane: FamilyPlane::Semantic,
        rel_path: path("a.txt"),
        content: ContentHash::of(b"alpha"),
        producer: "model-a".to_string(),
    };
    let completion = Completion {
        item,
        key: semantic_key("a.txt", b"alpha", "model-a"),
    };
    let mut fill = FillMap::default();
    assert_eq!(
        fill.admit(&completion, "model-b", None, Some(&manifest)),
        Admission::DroppedProducer
    );
    write(&mut disk, &mut delta, "a.txt", b"newer edit");
    assert_eq!(
        fill.admit(
            &completion,
            "model-a",
            delta.entry(&path("a.txt")).map(|entry| &entry.disk),
            Some(&manifest)
        ),
        Admission::DroppedContent
    );
    assert!(fill.is_empty());
}
