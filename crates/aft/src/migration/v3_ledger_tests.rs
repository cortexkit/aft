use std::path::Path;
use std::time::{Duration, Instant};

use super::*;
use crate::blob_store::v2::{ContentHash, FamilyKey, TrigramKey};
use crate::migration::v3_import::tests::{
    checkout, deterministic, legacy_semantic, legacy_trigram, producer, snapshot_tree, FAMILY, KEY,
    POLICY,
};
use crate::views::segment_store::TrigramPayload;

fn request(storage: &Path, root: &Path, semantic: Option<SemanticProducer>) -> V3ImportRequest {
    let mut request = V3ImportRequest::full_plane(
        storage.to_path_buf(),
        FAMILY.to_owned(),
        KEY.to_owned(),
        root.to_path_buf(),
        POLICY,
        semantic,
    );
    // The symbol artifact has its own tests; these cover the two others.
    request.symbols = false;
    request
}

fn outcome(report: &V3ImportReport, artifact: LegacyArtifact) -> ArtifactOutcome {
    report.outcomes[&artifact].clone()
}

fn row_of(storage: &Path, artifact: LegacyArtifact) -> V3ImportRow {
    read_rows(storage, FAMILY)
        .unwrap()
        .unwrap()
        .into_iter()
        .find(|row| row.key.artifact == artifact)
        .unwrap()
}

fn trigram_keys(
    root_sources: &BTreeMap<PathBuf, Vec<u8>>,
    policy: TrigramPolicy,
) -> Vec<FamilyKey> {
    root_sources
        .values()
        .map(|source| {
            TrigramKey {
                content: ContentHash::of(source),
                policy,
            }
            .family_key()
        })
        .collect()
}

fn present(storage: &Path, plane: FamilyPlane, keys: &[FamilyKey]) -> usize {
    if !v3::plane_path(storage, FAMILY, plane).unwrap().exists() {
        return 0;
    }
    let reader = v3::PlaneReader::open_existing(storage, FAMILY, plane).unwrap();
    keys.iter()
        .filter(|key| reader.contains(key).unwrap())
        .count()
}

/// A full import puts every compatible row into v3, calling no model and
/// reading no source for the trigram plane, and records one done row per
/// artifact; a second run does nothing. Legacy bytes stay identical.
#[test]
fn a_full_import_lands_every_artifact_and_a_second_run_is_already_complete() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    legacy_trigram(&storage, &root);
    legacy_semantic(&storage, &root);
    let legacy = snapshot_tree(&storage);
    let producer = producer();
    let report = run_v3_import(&request(&storage, &root, Some(producer.clone())), None).unwrap();
    for artifact in [LegacyArtifact::Trigram, LegacyArtifact::Semantic] {
        assert!(
            matches!(outcome(&report, artifact), ArtifactOutcome::Done { ref registered, .. } if registered.inserted > 0),
            "{artifact:?}: {:?}",
            outcome(&report, artifact)
        );
        let row = row_of(&storage, artifact);
        assert_eq!(
            (row.state, row.attempt, row.owner),
            (ImportState::Done, 1, None)
        );
    }
    // Every imported semantic payload is the one a cold fill stores under
    // the same key; the import itself had no model to call.
    let reader = v3::PlaneReader::open_existing(&storage, FAMILY, FamilyPlane::Semantic).unwrap();
    let payload_producer = crate::semantic_index::ViewPayloadProducer {
        chunker_version: &producer.chunker_version,
        template_version: &producer.template_version,
        model_fingerprint: &producer.model_fingerprint,
    };
    let mut model_calls = 0;
    let mut checked = 0;
    for path in ["src/alpha.rs", "src/beta.rs", "notes/readme.md"] {
        let rel_path = crate::views::RelPath::new(path.as_bytes().to_vec()).unwrap();
        if !crate::views::semantic::applies_to(&rel_path) {
            continue;
        }
        let source = fs::read(root.join(path)).unwrap();
        let chunks =
            crate::semantic_index::chunk_view_file(&root, Path::new(path), &source, producer.caps)
                .unwrap()
                .unwrap();
        let mut embed = |texts: Vec<String>| {
            model_calls += 1;
            Ok(texts.iter().map(|text| deterministic(text)).collect())
        };
        let runs = crate::semantic_index::embed_view_files(vec![chunks], &mut embed, 64).unwrap();
        assert_eq!(
            reader.get(&producer.key(&source, &rel_path)).unwrap(),
            Some(runs[0].encode_view_payload(&payload_producer)),
            "semantic row of {path}"
        );
        checked += 1;
    }
    assert!(
        checked >= 2 && model_calls >= checked,
        "{checked} {model_calls}"
    );
    let again = run_v3_import(&request(&storage, &root, Some(producer)), None).unwrap();
    assert_eq!(
        outcome(&again, LegacyArtifact::Trigram),
        ArtifactOutcome::AlreadyComplete(ImportState::Done)
    );
    assert!(again.steps.is_empty());
    let after = snapshot_tree(&storage);
    for (path, bytes) in &legacy {
        assert_eq!(after.get(path), Some(bytes), "{} changed", path.display());
    }
    assert!(snapshot_tree(&staging_dir(&storage, FAMILY, KEY)).is_empty());
    assert!(!storage.join("migration/v2").exists());
    assert!(!storage.join("blobs/v2").exists());
}

/// Completion is per artifact, producer and plane: a run that completed only
/// the semantic plane (the legacy set had no postings yet) leaves the trigram
/// import to a later run instead of standing for it.
#[test]
fn a_semantic_only_completion_does_not_suppress_a_later_trigram_import() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    legacy_semantic(&storage, &root);
    let request = request(&storage, &root, Some(producer()));
    let first = run_v3_import(&request, None).unwrap();
    assert_eq!(
        outcome(&first, LegacyArtifact::Trigram),
        ArtifactOutcome::Absent
    );
    assert!(matches!(
        outcome(&first, LegacyArtifact::Semantic),
        ArtifactOutcome::Done { .. }
    ));
    let status = legacy_key_status(&storage, KEY).unwrap().unwrap();
    assert!(!status.incomplete && status.completed_ms.is_some());

    legacy_trigram(&storage, &root);
    let second = run_v3_import(&request, None).unwrap();
    assert!(
        matches!(outcome(&second, LegacyArtifact::Trigram), ArtifactOutcome::Done { ref registered, .. } if registered.inserted > 0),
        "{:?}",
        second.outcomes
    );
    assert_eq!(
        outcome(&second, LegacyArtifact::Semantic),
        ArtifactOutcome::AlreadyComplete(ImportState::Done)
    );
    let sources = snapshot_tree(&root);
    assert!(
        present(
            &storage,
            FamilyPlane::Trigram,
            &trigram_keys(&sources, POLICY)
        ) >= 3
    );
}

/// A changed producer is a new row that has to prove compatibility again:
/// a cache built under another size limit or another model is rejected,
/// nothing is stored under the new producer's keys, and the earlier
/// producer's completed row stays as it was.
#[test]
fn a_changed_producer_starts_a_new_row_and_provably_misses() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    legacy_trigram(&storage, &root);
    legacy_semantic(&storage, &root);
    run_v3_import(&request(&storage, &root, Some(producer())), None).unwrap();
    let sources = snapshot_tree(&root);

    let other_policy = TrigramPolicy {
        max_file_size: 4096,
    };
    let mut other_fingerprint = crate::migration::v3_import::tests::fingerprint();
    other_fingerprint.model = "another".to_owned();
    let other_producer = SemanticProducer::current(
        other_fingerprint.as_string(),
        other_fingerprint.embed_text_caps,
    );
    let mut changed = request(&storage, &root, Some(other_producer.clone()));
    changed.trigram = other_policy;
    let report = run_v3_import(&changed, None).unwrap();
    for artifact in [LegacyArtifact::Trigram, LegacyArtifact::Semantic] {
        assert!(
            matches!(outcome(&report, artifact), ArtifactOutcome::Rejected(_)),
            "{artifact:?}: {:?}",
            outcome(&report, artifact)
        );
    }
    // Counted misses: no row exists under any key of the changed producers.
    assert_eq!(
        present(
            &storage,
            FamilyPlane::Trigram,
            &trigram_keys(&sources, other_policy)
        ),
        0
    );
    let semantic_keys = sources
        .iter()
        .map(|(path, source)| {
            let relative = path.strip_prefix(&root).unwrap();
            other_producer.key(
                source,
                &crate::views::RelPath::from_os_path(relative).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(present(&storage, FamilyPlane::Semantic, &semantic_keys), 0);
    // The original producer's rows are still the ones the first run stored.
    assert!(
        present(
            &storage,
            FamilyPlane::Trigram,
            &trigram_keys(&sources, POLICY)
        ) >= 3
    );
    let rows = read_rows(&storage, FAMILY).unwrap().unwrap();
    assert_eq!(rows.len(), 4, "{rows:#?}");
    assert_eq!(
        rows.iter()
            .filter(|row| row.state == ImportState::Done)
            .count(),
        2
    );
}

/// A refused reservation is the plane's named storage gap. The row keeps its
/// state with its claim released and the gap recorded, nothing reads the
/// legacy artifact in place of v3, and a later run with room resumes it.
#[test]
fn a_storage_gap_releases_the_row_and_a_later_run_resumes_it() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    legacy_trigram(&storage, &root);
    // Room for a 64 KiB ledger write above the 4 GiB floor, not for a batch.
    v3::set_free_space_for_test((4 << 30) + (512 << 10), 1 << 30);
    let report = run_v3_import(&request(&storage, &root, None), None);
    v3::set_free_space_for_test(u64::MAX, u64::MAX / 2);
    let report = report.unwrap();
    assert_eq!(
        outcome(&report, LegacyArtifact::Trigram),
        ArtifactOutcome::StorageGap("storage_gap:device_free_space_floor".to_owned())
    );
    let row = row_of(&storage, LegacyArtifact::Trigram);
    assert_eq!(row.state, ImportState::Validated);
    assert_eq!(row.owner, None);
    assert_eq!(
        row.reason.as_deref(),
        Some("storage_gap:device_free_space_floor")
    );
    let sources = snapshot_tree(&root);
    assert_eq!(
        present(
            &storage,
            FamilyPlane::Trigram,
            &trigram_keys(&sources, POLICY)
        ),
        0
    );

    let resumed = run_v3_import(&request(&storage, &root, None), None).unwrap();
    assert!(matches!(
        outcome(&resumed, LegacyArtifact::Trigram),
        ArtifactOutcome::Done { .. }
    ));
    let row = row_of(&storage, LegacyArtifact::Trigram);
    assert_eq!((row.state, row.attempt), (ImportState::Done, 2));
    assert!(
        present(
            &storage,
            FamilyPlane::Trigram,
            &trigram_keys(&sources, POLICY)
        ) >= 3
    );
}

/// A live claim is single flight; once its owner is gone the row is taken
/// over and finished.
#[test]
fn a_live_claim_is_single_flight_and_an_abandoned_one_is_taken_over() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    legacy_trigram(&storage, &root);
    let request = request(&storage, &root, None);
    let key = request.rows().remove(0);
    let ledger = V3Ledger::new(&storage, FAMILY).unwrap();
    let other = ClaimOwner::new();
    let active = ActiveClaim::register(&other);
    assert!(matches!(
        ledger.claim(&key, &other).unwrap(),
        Claim::Owned(_)
    ));
    let busy = run_v3_import(&request, None).unwrap();
    assert!(matches!(
        outcome(&busy, LegacyArtifact::Trigram),
        ArtifactOutcome::Busy { .. }
    ));
    let status = legacy_key_status(&storage, KEY).unwrap().unwrap();
    assert!(status.live_claim && status.incomplete);
    drop(active);
    let done = run_v3_import(&request, None).unwrap();
    assert!(matches!(
        outcome(&done, LegacyArtifact::Trigram),
        ArtifactOutcome::Done { .. }
    ));
    assert_eq!(row_of(&storage, LegacyArtifact::Trigram).attempt, 2);
}

#[test]
fn a_ledger_from_a_newer_build_is_refused_by_name() {
    let _serial = v3::serial_for_test();
    let dir = tempfile::tempdir().unwrap();
    let root = checkout(dir.path());
    let storage = dir.path().join("storage");
    legacy_trigram(&storage, &root);
    run_v3_import(&request(&storage, &root, None), None).unwrap();
    let path = V3Ledger::new(&storage, FAMILY)
        .unwrap()
        .path()
        .to_path_buf();
    // Close this process's shared connection before editing the file.
    lock(open_connections()).clear();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute("UPDATE format_meta SET format_version = 2", [])
        .unwrap();
    drop(connection);
    assert!(matches!(
        read_rows(&storage, FAMILY),
        Err(V3ImportError::NewerLedger(2))
    ));
}

// ---------------------------------------------------------------------------
// Crash transitions: a child process is killed with SIGKILL right after each
// durable boundary of the trigram import; the next run must finish the import
// from that state with nothing lost, duplicated or left in staging.

const KILL_SUBJECT: &str = "AFT_V3_IMPORT_KILL_SUBJECT";
const KILL_AT: &str = "AFT_V3_IMPORT_KILL_AT";

fn step_name(step: V3Step) -> String {
    match step {
        V3Step::BatchRegistered(batch) => format!("batch-{batch}"),
        other => format!("{other:?}").to_lowercase(),
    }
}

struct Park {
    at: String,
    ready: PathBuf,
}

impl V3ImportObserver for Park {
    fn reached(&self, artifact: LegacyArtifact, step: V3Step) {
        if artifact != LegacyArtifact::Trigram || step_name(step) != self.at {
            return;
        }
        fs::write(&self.ready, step_name(step)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("kill-test parent did not send SIGKILL within 30 seconds");
    }
}

#[cfg(unix)]
#[test]
fn v3_import_kill_child() {
    let Ok(subject) = std::env::var(KILL_SUBJECT) else {
        return;
    };
    let at = std::env::var(KILL_AT).unwrap();
    let subject = PathBuf::from(subject);
    v3::set_free_space_for_test(u64::MAX, u64::MAX / 2);
    let park = Park {
        at,
        ready: subject.join("ready"),
    };
    let root = subject.join("checkout");
    run_v3_import(&request(&subject.join("storage"), &root, None), Some(&park)).unwrap();
    panic!("the import finished without reaching the kill point");
}

#[cfg(unix)]
fn kill_child_at(subject: &Path, at: &str) {
    use std::os::unix::process::ExitStatusExt;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "migration::v3_ledger::tests::v3_import_kill_child",
            "--nocapture",
        ])
        .env(KILL_SUBJECT, subject)
        .env(KILL_AT, at)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !subject.join("ready").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "kill-test child exited before reaching {at}"
        );
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("kill-test child did not reach {at} within 60 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(9));
}

#[cfg(unix)]
#[test]
fn every_ledger_crash_transition_resumes_to_the_same_rows() {
    let _serial = v3::serial_for_test();
    let reference = {
        let dir = tempfile::tempdir().unwrap();
        let root = checkout(dir.path());
        let storage = dir.path().join("storage");
        legacy_trigram(&storage, &root);
        run_v3_import(&request(&storage, &root, None), None).unwrap();
        let reader =
            v3::PlaneReader::open_existing(&storage, FAMILY, FamilyPlane::Trigram).unwrap();
        let keys = trigram_keys(&snapshot_tree(&root), POLICY)
            .into_iter()
            .filter(|key| reader.contains(key).unwrap())
            .collect::<Vec<_>>();
        assert!(keys.len() >= 3);
        (keys, row_of(&storage, LegacyArtifact::Trigram).imported)
    };
    for at in [
        "claimed",
        "staged",
        "validated",
        "batch-1",
        "registered",
        "done",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let subject = dir.path();
        let root = checkout(subject);
        let storage = subject.join("storage");
        legacy_trigram(&storage, &root);
        let legacy = snapshot_tree(&storage);
        kill_child_at(subject, at);
        let killed = row_of(&storage, LegacyArtifact::Trigram);
        assert_eq!(
            killed.owner.is_some(),
            at != "done",
            "{at}: the killed child held the row until it was done"
        );

        let report = run_v3_import(&request(&storage, &root, None), None).unwrap();
        let expected = if at == "done" {
            ArtifactOutcome::AlreadyComplete(ImportState::Done)
        } else {
            match outcome(&report, LegacyArtifact::Trigram) {
                done @ ArtifactOutcome::Done { .. } => done,
                other => panic!("{at}: {other:?}"),
            }
        };
        assert_eq!(outcome(&report, LegacyArtifact::Trigram), expected, "{at}");
        let row = row_of(&storage, LegacyArtifact::Trigram);
        assert_eq!(row.state, ImportState::Done, "{at}");
        assert_eq!(row.attempt, if at == "done" { 1 } else { 2 }, "{at}");
        assert_eq!(row.imported, reference.1, "{at}");
        assert_eq!(
            present(&storage, FamilyPlane::Trigram, &reference.0),
            reference.0.len(),
            "{at}: every row is in v3"
        );
        let reader =
            v3::PlaneReader::open_existing(&storage, FAMILY, FamilyPlane::Trigram).unwrap();
        for (_, source) in snapshot_tree(&root) {
            let key = TrigramKey {
                content: ContentHash::of(&source),
                policy: POLICY,
            }
            .family_key();
            if !reference.0.contains(&key) {
                continue;
            }
            assert_eq!(
                reader.get(&key).unwrap(),
                Some(TrigramPayload::extract(&source, &POLICY).encode()),
                "{at}"
            );
        }
        assert!(
            snapshot_tree(&staging_dir(&storage, FAMILY, KEY)).is_empty(),
            "{at}: staging left behind"
        );
        let after = snapshot_tree(&storage);
        for (path, bytes) in &legacy {
            assert_eq!(
                after.get(path),
                Some(bytes),
                "{at}: {} changed",
                path.display()
            );
        }
    }
}
