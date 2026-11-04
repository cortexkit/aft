use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::cache::BlockCache;
use crate::ids::Digest128;
use crate::segment::{
    Alignment, BLOCK_LEN, KeySpace, KnownSection, Requirement, SectionId, SectionKind, SectionSpec,
};

const KIND: SectionKind = SectionKind::new(1);

fn family_id() -> FamilyId {
    FamilyId::from_digest(Digest128::of(b"repo"))
}

fn checkout(name: &str) -> CheckoutId {
    CheckoutId::from_digest(Digest128::of(name.as_bytes()))
}

fn known() -> Vec<KnownSection> {
    vec![KnownSection {
        kind: KIND,
        version: 1,
    }]
}

fn cache() -> Arc<BlockCache> {
    Arc::new(BlockCache::new(4 * BLOCK_LEN))
}

fn open(dir: &Path) -> Family {
    Family::open(dir, family_id()).unwrap()
}

/// Writes one segment holding `bytes` and returns its id, staged.
fn segment(session: &mut WriteSession<'_>, bytes: &[u8]) -> Result<SegmentId, FamilyError> {
    let mut pending = session.create_segment()?;
    pending.writer().write_section(
        SectionSpec {
            id: SectionId::new(KIND, KeySpace::new(0)),
            version: 1,
            alignment: Alignment::Word,
            requirement: Requirement::Required,
        },
        bytes,
        1,
    )?;
    session.finish_segment(pending)
}

/// Publishes one generation that replaces every segment with a new one
/// holding `bytes`, and sets `checkout`'s state to `state`.
fn publish(family: &Family, bytes: &[u8], state: &[u8]) -> Generation {
    let mut session = family.try_write().unwrap().unwrap();
    let id = segment(&mut session, bytes).unwrap();
    let state = session.write_state(checkout("main"), state).unwrap();
    let old = session
        .base()
        .segments()
        .iter()
        .map(|entry| entry.id)
        .collect();
    session
        .publish(&Publication {
            add_segments: vec![id],
            remove_segments: old,
            states: vec![state],
            retire_checkouts: Vec::new(),
        })
        .unwrap()
}

fn read_section(snapshot: &Snapshot) -> Vec<u8> {
    let id = snapshot.manifest().segments()[0].id;
    let segment = snapshot.open_segment(id, &known(), cache()).unwrap();
    let section = segment
        .section(SectionId::new(KIND, KeySpace::new(0)))
        .unwrap();
    let len = segment.info(section).len;
    segment.read_range(section, 0..len).unwrap()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn an_empty_family_reads_as_the_empty_generation() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    let snapshot = family.snapshot().unwrap();
    assert_eq!(snapshot.generation(), Generation::EMPTY);
    assert!(snapshot.manifest().segments().is_empty());
    assert_eq!(snapshot.read_state(checkout("main")).unwrap(), None);
}

#[test]
fn a_published_generation_reads_back_whole() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    assert_eq!(publish(&family, b"first", b"state one"), Generation::new(1));
    assert_eq!(
        publish(&family, b"second", b"state two"),
        Generation::new(2)
    );
    let snapshot = family.snapshot().unwrap();
    assert_eq!(snapshot.generation(), Generation::new(2));
    assert_eq!(read_section(&snapshot), b"second");
    assert_eq!(
        snapshot.read_state(checkout("main")).unwrap().as_deref(),
        Some(&b"state two"[..])
    );
}

#[test]
fn one_writer_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    let other = open(dir.path());
    let session = family.try_write().unwrap().unwrap();
    assert!(other.try_write().unwrap().is_none());
    drop(session);
    assert!(other.try_write().unwrap().is_some());
}

#[test]
fn a_held_snapshot_keeps_its_generation_until_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"old", b"old state");
    let held = family.snapshot().unwrap();
    publish(&family, b"new", b"new state");

    let mut session = family.try_write().unwrap().unwrap();
    let report = session.reclaim().unwrap();
    assert_eq!(report.outcome, ReclaimOutcome::Complete);
    assert_eq!(report.horizon, Generation::new(1));
    assert_eq!(report.removed_segments, 0);
    assert_eq!(read_section(&held), b"old");
    assert_eq!(
        held.read_state(checkout("main")).unwrap().as_deref(),
        Some(&b"old state"[..])
    );

    drop(held);
    let report = session.reclaim().unwrap();
    assert_eq!(report.outcome, ReclaimOutcome::Complete);
    assert_eq!(report.horizon, Generation::new(2));
    assert_eq!(report.removed_segments, 1);
    assert_eq!(report.removed_states, 1);
    assert_eq!(report.removed_manifests, 1);
    drop(session);
    assert_eq!(names(&family.layout.segments_dir()), ["2.seg"]);
    assert_eq!(
        names(&family.layout.checkout_dir(checkout("main"))),
        ["STATE-2"]
    );
    assert_eq!(read_section(&family.snapshot().unwrap()), b"new");
}

#[test]
fn a_reader_behind_the_horizon_starts_again_on_a_newer_generation() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"old", b"s");
    publish(&family, b"new", b"s");
    // A horizon above every generation but the newest, as a reclamation
    // that has not removed anything yet leaves it.
    let mut session = family.try_write().unwrap().unwrap();
    assert_eq!(session.reclaim().unwrap().horizon, Generation::new(2));
    drop(session);
    let snapshot = family.snapshot().unwrap();
    assert_eq!(snapshot.generation(), Generation::new(2));
}

#[test]
fn torn_manifests_are_skipped_and_never_reused() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"one", b"s");
    // A publication torn by a crash: the name exists, the bytes do not check.
    std::fs::write(family.layout.manifest(Generation::new(2)), b"torn").unwrap();
    let snapshot = family.snapshot().unwrap();
    assert_eq!(snapshot.generation(), Generation::new(1));
    assert_eq!(snapshot.skipped_manifests(), 1);
    drop(snapshot);
    // The next writer removes it and publishes past its number.
    assert_eq!(publish(&family, b"two", b"s"), Generation::new(3));
    assert!(!family.layout.manifest(Generation::new(2)).exists());
}

#[test]
fn a_damaged_segment_or_state_is_reported_not_guessed() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"bytes", b"state");
    let snapshot = family.snapshot().unwrap();
    let id = snapshot.manifest().segments()[0].id;
    // Appending changes the length the manifest published.
    let path = family.layout.segment(id);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.push(0);
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        snapshot.open_segment(id, &known(), cache()),
        Err(FamilyError::Damaged { .. })
    ));
    let state = family.layout.state(checkout("main"), 1);
    std::fs::remove_file(&state).unwrap();
    std::fs::write(&state, b"not a record").unwrap();
    assert!(matches!(
        snapshot.read_state(checkout("main")),
        Err(FamilyError::Damaged { .. })
    ));
}

#[test]
fn files_of_ended_processes_are_removed_and_protect_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"old", b"s");
    publish(&family, b"new", b"s");
    // An ended process: its owner file is unlocked, and its lease names the
    // old generation.
    let leases = family.layout.leases_dir();
    let owner = "0".repeat(32);
    std::fs::write(leases.join(format!("{owner}.owner")), b"").unwrap();
    record::write(
        &leases.join(format!("{owner}.1.lease")),
        Kind::Lease,
        family_id(),
        1,
        &1u64.to_le_bytes(),
    )
    .unwrap();
    // A lease whose owner file is gone altogether.
    record::write(
        &leases.join(format!("{}.1.lease", "1".repeat(32))),
        Kind::Lease,
        family_id(),
        1,
        &1u64.to_le_bytes(),
    )
    .unwrap();

    let mut session = family.try_write().unwrap().unwrap();
    let report = session.reclaim().unwrap();
    assert_eq!(report.outcome, ReclaimOutcome::Complete);
    assert_eq!(report.horizon, Generation::new(2));
    assert_eq!(report.removed_leases, 3);
    assert_eq!(report.removed_segments, 1);
    assert!(names(&leases).is_empty());
}

#[test]
fn an_unreadable_live_lease_stops_reclamation() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"old", b"s");
    let held = family.snapshot().unwrap();
    publish(&family, b"new", b"s");
    // A live process's lease that cannot be read: this process's owner file
    // is locked, so its records count as live.
    let owner = names(&family.layout.leases_dir())
        .into_iter()
        .find_map(|name| name.strip_suffix(".owner").map(str::to_string))
        .unwrap();
    std::fs::write(
        family.layout.leases_dir().join(format!("{owner}.99.lease")),
        b"half written",
    )
    .unwrap();
    let mut session = family.try_write().unwrap().unwrap();
    let report = session.reclaim().unwrap();
    assert_eq!(report.outcome, ReclaimOutcome::UnreadableLeases(1));
    assert_eq!(report.removed_segments, 0);
    assert_eq!(read_section(&held), b"old");
}

#[test]
fn a_lease_below_the_written_horizon_stops_reclamation() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"one", b"s");
    publish(&family, b"two", b"s");
    let mut session = family.try_write().unwrap().unwrap();
    assert_eq!(session.reclaim().unwrap().horizon, Generation::new(2));
    // A reader that leased generation 1 before seeing the horizon; it will
    // start again, and until it does nothing below the horizon may go.
    let owner = family.owner().unwrap();
    let lease = owner.lease(family_id(), Generation::new(1)).unwrap();
    let report = session.reclaim().unwrap();
    assert_eq!(report.outcome, ReclaimOutcome::Raced);
    assert_eq!(report.horizon, Generation::new(2));
    drop(lease);
    assert_eq!(session.reclaim().unwrap().outcome, ReclaimOutcome::Complete);
}

#[test]
fn publication_refuses_what_the_session_did_not_stage() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"one", b"s");
    let mut session = family.try_write().unwrap().unwrap();
    for change in [
        Publication {
            add_segments: vec![SegmentId::new(99)],
            ..Publication::default()
        },
        Publication {
            remove_segments: vec![SegmentId::new(99)],
            ..Publication::default()
        },
        Publication {
            states: vec![StateRef {
                checkout: checkout("main"),
                number: 1,
                len: 1,
                digest: Digest128::of(b"s"),
            }],
            ..Publication::default()
        },
    ] {
        assert!(matches!(
            session.publish(&change),
            Err(FamilyError::Misuse(_))
        ));
    }
}

/// Crashes a writer at every file step in turn and checks, after each, that
/// the family reopens to its newest complete generation with every file it
/// names intact, that the next writer publishes normally, and that a full
/// reclamation then leaves only the files the newest generation names.
#[test]
fn a_crash_at_any_step_leaves_the_newest_complete_generation_readable() {
    let mut steps_without_crash = None;
    for crash_at in 0.. {
        let dir = tempfile::tempdir().unwrap();
        let family = open(dir.path());
        publish(&family, b"base", b"base state");
        // A second checkout, retired by the crashing publication.
        {
            let mut session = family.try_write().unwrap().unwrap();
            let state = session
                .write_state(checkout("worktree"), b"worktree state")
                .unwrap();
            session
                .publish(&Publication {
                    states: vec![state],
                    ..Publication::default()
                })
                .unwrap();
        }
        let held = family.snapshot().unwrap();

        crate::fault::reset_steps();
        crate::fault::fail_after(crash_at);
        let crashed = (|| -> Result<(), FamilyError> {
            let mut session = family.try_write()?.ok_or(FamilyError::Misuse("locked"))?;
            let id = segment(&mut session, b"next")?;
            let state = session.write_state(checkout("main"), b"next state")?;
            let old = session
                .base()
                .segments()
                .iter()
                .map(|entry| entry.id)
                .collect();
            session.publish(&Publication {
                add_segments: vec![id],
                remove_segments: old,
                states: vec![state],
                retire_checkouts: vec![checkout("worktree")],
            })?;
            drop(held);
            session.reclaim()?;
            Ok(())
        })();
        let fired = crate::fault::fired();
        crate::fault::disarm();
        let steps = crate::fault::steps();

        // Whatever happened, the family reads as one complete generation.
        let snapshot = family.snapshot().unwrap();
        let generation = snapshot.generation();
        assert!(
            generation == Generation::new(2) || generation == Generation::new(3),
            "crash at {crash_at}: generation {generation}"
        );
        let expected = if generation == Generation::new(3) {
            (&b"next"[..], &b"next state"[..])
        } else {
            (&b"base"[..], &b"base state"[..])
        };
        assert_eq!(read_section(&snapshot), expected.0, "crash at {crash_at}");
        assert_eq!(
            snapshot.read_state(checkout("main")).unwrap().as_deref(),
            Some(expected.1),
            "crash at {crash_at}"
        );
        assert_eq!(
            snapshot.read_state(checkout("worktree")).unwrap().is_some(),
            generation == Generation::new(2),
            "crash at {crash_at}"
        );
        drop(snapshot);

        // The next writer publishes normally, and a full reclamation leaves
        // only what the newest generation names.
        let after = publish(&family, b"after", b"after state");
        let mut session = family.try_write().unwrap().unwrap();
        assert_eq!(session.reclaim().unwrap().outcome, ReclaimOutcome::Complete);
        drop(session);
        let snapshot = family.snapshot().unwrap();
        assert_eq!(snapshot.generation(), after);
        let named = format!("{}.seg", snapshot.manifest().segments()[0].id);
        assert_eq!(
            names(&family.layout.segments_dir()),
            [named],
            "crash at {crash_at}"
        );
        assert!(
            names(&family.layout.tmp_dir()).is_empty(),
            "crash at {crash_at}"
        );
        let mut checkouts = vec![checkout("main").to_string()];
        if generation == Generation::new(2) {
            checkouts.push(checkout("worktree").to_string());
            checkouts.sort();
        }
        assert_eq!(
            names(&family.layout.checkouts_dir()),
            checkouts,
            "crash at {crash_at}"
        );
        assert_eq!(
            names(&family.layout.dir)
                .into_iter()
                .filter(|name| name.starts_with(MANIFEST_PREFIX))
                .collect::<Vec<_>>(),
            [format!("{MANIFEST_PREFIX}{after}")],
            "crash at {crash_at}"
        );

        if !fired {
            crashed.unwrap();
            steps_without_crash = Some(steps);
            break;
        }
    }
    let steps = steps_without_crash.unwrap();
    println!("crash points tried: {steps}");
    assert!(steps > 10, "the writer passed only {steps} file steps");
}
