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

/// Every checkout of a repository reads the same segment files, so a
/// process opens each once: two snapshots naming the same segment share
/// its open file while either holds it, and the file closes when the last
/// holder drops it.
#[test]
fn a_segment_file_is_opened_once_for_every_snapshot_that_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"shared bytes", b"state");
    let shared = cache();
    let first = family.snapshot().unwrap();
    let second = family.snapshot().unwrap();
    let id = first.manifest().segments()[0].id;
    let a = first.open_shared_segment(id, &known(), &shared).unwrap();
    let b = second.open_shared_segment(id, &known(), &shared).unwrap();
    assert!(Arc::ptr_eq(&a, &b), "both snapshots read one open file");

    // A segment cached in another cache is opened on its own: its blocks
    // would otherwise land in a cache its caller does not read.
    let elsewhere = first.open_shared_segment(id, &known(), &cache()).unwrap();
    assert!(!Arc::ptr_eq(&a, &elsewhere));

    // Once every holder drops it, the registry does not keep it open.
    let weak = Arc::downgrade(&a);
    drop((a, b, elsewhere));
    assert!(
        weak.upgrade().is_none(),
        "the file closed with its last holder"
    );
    let again = second.open_shared_segment(id, &known(), &shared).unwrap();
    assert_eq!(again.id(), id);

    // A segment the snapshot does not name is refused, as unshared.
    publish(&family, b"newer bytes", b"state");
    let newer = family.snapshot().unwrap();
    let newer_id = newer.manifest().segments()[0].id;
    assert!(
        first
            .open_shared_segment(newer_id, &known(), &shared)
            .is_err()
    );
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
fn a_manifest_or_horizon_still_being_written_is_skipped_like_a_torn_one() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"one", b"s");
    // A publication and a reclamation caught mid-write: the writer still
    // holds each file open for writing, as `record::write` does until the
    // bytes are synced. Windows refuses to open a file for a reader that
    // denies writers while a writer has it open.
    let mut manifest =
        crate::fs::create_new_private(&family.layout.manifest(Generation::new(2))).unwrap();
    manifest.write_all(b"half a manifest").unwrap();
    let mut horizon = crate::fs::create_new_private(&family.layout.horizon(1)).unwrap();
    horizon.write_all(b"half").unwrap();

    let snapshot = family.snapshot().unwrap();
    assert_eq!(snapshot.generation(), Generation::new(1));
    assert_eq!(snapshot.skipped_manifests(), 1);
    assert_eq!(read_section(&snapshot), b"one");
    drop((manifest, horizon));
}

#[test]
fn a_segment_a_reader_still_has_open_is_reclaimed_and_stays_readable() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"old", b"s");
    let snapshot = family.snapshot().unwrap();
    let id = snapshot.manifest().segments()[0].id;
    // Nothing read yet, so every later read goes to the file.
    let segment = snapshot.open_segment(id, &known(), cache()).unwrap();
    drop(snapshot);
    publish(&family, b"new", b"s");

    // The open handle neither stops the removal (on Windows it allows
    // deletion) nor loses its bytes to it.
    let mut session = family.try_write().unwrap().unwrap();
    let report = session.reclaim().unwrap();
    assert_eq!(report.outcome, ReclaimOutcome::Complete);
    assert_eq!((report.removed_segments, report.failed_removals), (1, 0));
    let section = segment
        .section(SectionId::new(KIND, KeySpace::new(0)))
        .unwrap();
    let len = segment.info(section).len;
    assert_eq!(segment.read_range(section, 0..len).unwrap(), b"old");
    drop(segment);
    drop(session);
    assert_eq!(names(&family.layout.segments_dir()), ["2.seg"]);
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

/// A reader that binds while a reclaimer scans keeps its lease and its data.
/// The reader takes its snapshot between the scan's lease and owner
/// listings, the point a scan that listed owners first got wrong: it found
/// the new lease, missed its owner, and deleted both the lease and, on the
/// next reclaim, the segment the reader still held.
#[test]
fn a_reader_binding_during_a_scan_keeps_its_lease_and_data() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let dir = tempfile::tempdir().unwrap();
    let writer_family = open(dir.path());
    publish(&writer_family, b"first", b"state one");
    let reader_family = Rc::new(open(dir.path()));
    let held = Rc::new(RefCell::new(None));
    let saved = held.clone();
    super::lease::BETWEEN_LISTINGS.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            *saved.borrow_mut() = Some(reader_family.snapshot().unwrap());
        }));
    });
    let first = writer_family
        .try_write()
        .unwrap()
        .unwrap()
        .reclaim()
        .unwrap();
    let snapshot = held
        .borrow_mut()
        .take()
        .expect("the reader bound during the scan");
    assert_eq!(first.removed_leases, 0, "a live reader's lease was removed");
    assert_eq!(read_section(&snapshot), b"first");
    publish(&writer_family, b"second", b"state two");
    writer_family
        .try_write()
        .unwrap()
        .unwrap()
        .reclaim()
        .unwrap();
    assert_eq!(
        read_section(&snapshot),
        b"first",
        "the held generation's segment was reclaimed"
    );
}

#[test]
fn the_newest_generation_is_read_without_a_lease() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    assert_eq!(family.newest_generation().unwrap(), Generation::EMPTY);
    let first = publish(&family, b"one", b"state one");
    assert_eq!(family.newest_generation().unwrap(), first);
    let leases = || {
        std::fs::read_dir(dir.path().join("leases"))
            .unwrap()
            .count()
    };
    let before = leases();
    for _ in 0..3 {
        family.newest_generation().unwrap();
    }
    assert_eq!(leases(), before, "asking writes no lease");
    // A snapshot of the same generation does write one.
    let held = family.snapshot().unwrap();
    assert!(leases() > before);
    drop(held);
    let second = publish(&family, b"two", b"state two");
    assert!(second > first);
    assert_eq!(family.newest_generation().unwrap(), second);
}

/// Rewrites the record at `from` as a newer or older build would have
/// written it: `version` in the kind's format version field (bytes 10..12),
/// `number` in the number field (bytes 32..40), and the checksum over both,
/// so its bytes check. Writes it to `to`, replacing any file there.
fn rewrite_record(from: &Path, to: &Path, version: u16, number: u64) -> Vec<u8> {
    let mut bytes = std::fs::read(from).unwrap();
    bytes[10..12].copy_from_slice(&version.to_le_bytes());
    bytes[32..40].copy_from_slice(&number.to_le_bytes());
    let body = bytes.len() - Digest128::LEN;
    let checksum = Digest128::of(&bytes[..body]);
    bytes[body..].copy_from_slice(checksum.as_bytes());
    let _ = std::fs::remove_file(to);
    std::fs::write(to, &bytes).unwrap();
    bytes
}

/// Rewrites the segment format version (bytes 8..10) of the segment at
/// `path`, leaving its length as the manifest published it.
fn rewrite_segment_version(path: &Path, version: u16) -> Vec<u8> {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[8..10].copy_from_slice(&version.to_le_bytes());
    std::fs::remove_file(path).unwrap();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

fn assert_newer(error: &FamilyError, path: &Path, file: FileKind, found: u16) {
    assert_newer_than(error, path, file, found, 1);
}

fn assert_newer_than(error: &FamilyError, path: &Path, file: FileKind, found: u16, supported: u16) {
    assert!(!error.is_unusable_file(), "refused, not unusable: {error}");
    assert_eq!(
        error.newer_format(),
        Some(&NewerFormat {
            path: path.to_path_buf(),
            file,
            found,
            supported,
        }),
        "{error}"
    );
}

#[test]
fn a_manifest_a_newer_build_wrote_is_refused_and_nothing_is_changed() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"one", b"state");
    let planted = family.layout.manifest(Generation::new(2));
    let bytes = rewrite_record(&family.layout.manifest(Generation::new(1)), &planted, 2, 2);
    // A segment an earlier writer left unpublished, which starting a writer
    // would otherwise remove.
    let leftover = family.layout.tmp_segment(SegmentId::new(9));
    std::fs::write(&leftover, b"leftover").unwrap();

    assert_newer(
        &family.snapshot().unwrap_err(),
        &planted,
        FileKind::Record,
        2,
    );
    assert_newer(
        &family.check_formats().unwrap_err(),
        &planted,
        FileKind::Record,
        2,
    );
    assert_newer(
        &family.try_write().expect_err("the writer is refused"),
        &planted,
        FileKind::Record,
        2,
    );
    assert_eq!(std::fs::read(&planted).unwrap(), bytes);
    assert!(leftover.exists(), "a refused writer removed nothing");
}

#[test]
fn a_manifest_an_older_build_wrote_stays_unusable() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"one", b"state");
    let planted = family.layout.manifest(Generation::new(2));
    rewrite_record(&family.layout.manifest(Generation::new(1)), &planted, 0, 2);
    let error = family.snapshot().unwrap_err();
    assert!(matches!(error, FamilyError::Foreign { .. }), "{error}");
    assert!(error.is_unusable_file());
    assert_eq!(error.newer_format(), None);
}

#[test]
fn a_segment_a_newer_build_wrote_is_refused_and_never_reclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"one", b"state");
    let snapshot = family.snapshot().unwrap();
    let id = snapshot.manifest().segments()[0].id;
    let path = family.layout.segment(id);
    let bytes = rewrite_segment_version(&path, 2);

    assert_newer(
        &snapshot.open_segment(id, &known(), cache()).unwrap_err(),
        &path,
        FileKind::Segment,
        2,
    );
    drop(snapshot);
    assert_newer(
        &family.check_formats().unwrap_err(),
        &path,
        FileKind::Segment,
        2,
    );
    // No writer starts, so nothing can publish a generation without the
    // segment or reclaim it.
    assert_newer(
        &family.try_write().expect_err("the writer is refused"),
        &path,
        FileKind::Segment,
        2,
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn a_segment_an_older_build_wrote_stays_unusable_and_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"one", b"state");
    let snapshot = family.snapshot().unwrap();
    let id = snapshot.manifest().segments()[0].id;
    let path = family.layout.segment(id);
    rewrite_segment_version(&path, 0);
    let error = snapshot.open_segment(id, &known(), cache()).unwrap_err();
    assert!(error.is_unusable_file(), "{error}");
    assert_eq!(error.newer_format(), None);
    drop(snapshot);
    family.check_formats().unwrap();

    // A writer still starts, replaces the segment, and reclaims it.
    publish(&family, b"two", b"state");
    let mut session = family.try_write().unwrap().unwrap();
    assert_eq!(session.reclaim().unwrap().removed_segments, 1);
    assert!(!path.exists());
}

#[test]
fn a_checkout_state_a_newer_build_wrote_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"one", b"state");
    let path = family.layout.state(checkout("main"), 1);
    let bytes = rewrite_record(&path, &path, 2, 1);
    // The manifest named the state's payload, which is unchanged.
    let snapshot = family.snapshot().unwrap();
    assert_newer(
        &snapshot.read_state(checkout("main")).unwrap_err(),
        &path,
        FileKind::Record,
        2,
    );
    drop(snapshot);
    assert_newer(
        &family.check_formats().unwrap_err(),
        &path,
        FileKind::Record,
        2,
    );
    assert_newer(
        &family.try_write().expect_err("the writer is refused"),
        &path,
        FileKind::Record,
        2,
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

/// A checkout state payload labelled with state layout `format`, holding
/// `languages` paths that each name a language of their own, so the
/// payload's string table grows with them.
fn state_payload(format: u8, languages: usize) -> Vec<u8> {
    use crate::checkout::{CheckoutState, PathKind, PathRecord, StatEvidence};
    use crate::ids::ContentKey;
    let records = (0..languages)
        .map(|index| PathRecord {
            path: format!("file-{index}").into_bytes(),
            mode: 0o100_644,
            kind: PathKind::Text,
            content: ContentKey::of(b"text"),
            link: Vec::new(),
            stat: StatEvidence::default(),
            language: format!("{index:0>200}"),
        })
        .collect();
    CheckoutState::new(Vec::new(), Digest128::of(b"conversion"), records)
        .unwrap()
        .encode_as(format)
}

/// A state record this build reads, holding a state layout a newer build
/// wrote: refused by that layout's version, whether the record's first
/// bytes show it or its string table runs past them.
#[test]
fn a_checkout_state_payload_a_newer_build_wrote_is_refused() {
    for languages in [0, 400] {
        let dir = tempfile::tempdir().unwrap();
        let family = open(dir.path());
        publish(&family, b"one", &state_payload(4, languages));
        let path = family.layout.state(checkout("main"), 1);
        let bytes = std::fs::read(&path).unwrap();
        if languages > 0 {
            assert!(bytes.len() as u64 > crate::checkout::FORMAT_PEEK);
        }

        let snapshot = family.snapshot().unwrap();
        // The record itself checks: only the layout its payload names is
        // newer.
        assert!(snapshot.read_state(checkout("main")).unwrap().is_some());
        assert_newer_than(
            &snapshot.checkout_state(checkout("main")).unwrap_err(),
            &path,
            FileKind::Record,
            4,
            3,
        );
        drop(snapshot);
        assert_newer_than(
            &family.check_formats().unwrap_err(),
            &path,
            FileKind::Record,
            4,
            3,
        );
        assert_newer_than(
            &family.try_write().expect_err("the writer is refused"),
            &path,
            FileKind::Record,
            4,
            3,
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "{languages} languages"
        );
    }
}

/// A state layout this build reads decodes, and one older than any it reads
/// stays unusable, as before: the checkout is built again.
#[test]
fn a_checkout_state_payload_an_older_build_wrote_stays_unusable() {
    let dir = tempfile::tempdir().unwrap();
    let family = open(dir.path());
    publish(&family, b"one", &state_payload(3, 1));
    assert!(
        family
            .snapshot()
            .unwrap()
            .checkout_state(checkout("main"))
            .unwrap()
            .is_some()
    );
    publish(&family, b"two", &state_payload(1, 1));
    let error = family
        .snapshot()
        .unwrap()
        .checkout_state(checkout("main"))
        .unwrap_err();
    assert!(matches!(error, FamilyError::Damaged { .. }), "{error}");
    assert!(error.is_unusable_file());
    assert_eq!(error.newer_format(), None);
    family.check_formats().unwrap();
    assert!(
        family.try_write().unwrap().is_some(),
        "a writer still starts"
    );
}
