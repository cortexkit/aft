//! Tree-keyed bases through the checkout driver: which binds may skip a
//! member's read on its blob receipt, and which must take the full check.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use crate::blob_store::v2::{FamilyPlane, TrigramPolicy};
use crate::views::base_refs::{base_kind, open_foreign_v3, BaseKind};
use crate::views::first_load::{
    CheckoutDriver, ConfiguredMembershipWalker, FirstLoadDriver, ReconcileWork, TrigramBridge,
};
use crate::views::manifest_v2::Producers;
use crate::views::snapshot::{DiskState, Snapshot};
use crate::views::verification::{
    evidence_path, known_attribute_sources_for_test, read_tree_proof, write_tree_proof,
    BlobReceipt, FileIdentity, NativeStat, TreeProof, TREE_PROOF_APP, TREE_PROOF_FILE,
};
use crate::views::RelPath;

const FAMILY: &str = "family";
const WORKER: &str = "worker";
const FILES: [(&str, &[u8]); 2] = [
    ("file.rs", b"fn tree_member() { let marker = 41; }\n"),
    ("lib.rs", b"pub fn other_member() -> u8 { 7 }\n"),
];

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command.current_dir(root).args(args);
    crate::test_env::apply_hermetic_git_env(&mut command);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Writes `bytes` with an mtime well before any observation second, so a
/// stable read of it is reusable evidence.
fn write_old(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(60)),
        )
        .unwrap();
}

/// A checkout of `FILES`; committed (clean) when `commit` is set, otherwise
/// a plain directory without Git.
fn checkout(commit: bool) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (name, bytes) in FILES {
        write_old(&root.path().join(name), bytes);
    }
    if commit {
        git(root.path(), &["init", "-q", "--object-format=sha1"]);
        git(
            root.path(),
            &["config", "user.email", "tree@example.invalid"],
        );
        git(root.path(), &["config", "user.name", "Tree Base"]);
        git(root.path(), &["config", "core.autocrlf", "false"]);
        git(root.path(), &["add", "."]);
        git(
            root.path(),
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "fixture",
            ],
        );
    }
    root
}

fn policy() -> TrigramPolicy {
    TrigramPolicy {
        max_file_size: 1 << 20,
    }
}

fn producers() -> Producers {
    Producers {
        trigram: policy().fingerprint_hex(),
        semantic: None,
        callgraph: "unregistered".into(),
    }
}

fn driver(root: &Path, storage: &Path) -> CheckoutDriver {
    let plane = Arc::new(TrigramBridge::new(storage.to_owned(), policy()));
    CheckoutDriver::new_v3(
        storage,
        FAMILY,
        WORKER,
        root,
        producers(),
        Arc::new(ConfiguredMembershipWalker),
        vec![plane.clone()],
    )
    .unwrap()
    .with_adapters(vec![plane.adapter.clone()])
}

/// The first bind of a fresh checkout: bootstraps a base and publishes the
/// worker over it. Returns the base scope.
fn bootstrap(root: &Path, storage: &Path) -> String {
    let first = driver(root, storage);
    let access = first.access();
    let empty = first.installed_snapshot().generation().clone();
    let (snapshot, revision) = first.verified_bind(&access, empty).unwrap();
    first.install(&access, &snapshot, revision).unwrap();
    snapshot.generation().base_header().unwrap().scope.clone()
}

/// A later bind of the same checkout by a fresh driver (as after a restart).
fn rebind(root: &Path, storage: &Path) -> (Snapshot, ReconcileWork) {
    let next = driver(root, storage);
    let published = open_foreign_v3(storage, FAMILY, WORKER, &producers())
        .unwrap()
        .unwrap();
    let (snapshot, _) = next.verified_bind(&next.access(), published).unwrap();
    (snapshot, next.take_reconcile_work())
}

fn member_dir(storage: &Path, scope: &str) -> std::path::PathBuf {
    crate::blob_store::v3::view_dir(storage, FAMILY, scope).unwrap()
}

/// Removes the worker generation's own stat evidence, so only a base's
/// receipts could avoid a read.
fn remove_worker_evidence(storage: &Path) {
    let dir = member_dir(storage, WORKER);
    let generation = crate::views::ViewStore::existing_dir(dir.clone())
        .unwrap()
        .current_generation_read_only()
        .unwrap()
        .unwrap();
    std::fs::remove_file(evidence_path(&dir, &generation)).unwrap();
}

fn rel(name: &str) -> RelPath {
    RelPath::new(name.as_bytes().to_vec()).unwrap()
}

fn assert_current_bytes(snapshot: &Snapshot, root: &Path) {
    for (name, _) in FILES {
        assert_eq!(
            snapshot.disk_state(&rel(name)),
            DiskState::of_bytes(&std::fs::read(root.join(name)).unwrap()),
            "{name}"
        );
        assert!(matches!(
            snapshot.plane_readiness(&rel(name), FamilyPlane::Trigram),
            Some(crate::views::readiness::PlaneState::Ready { .. })
        ));
    }
}

#[test]
fn a_proven_tree_base_takes_the_receipt_fast_path() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let root = checkout(true);
    let storage = tempfile::tempdir().unwrap();
    let _attributes = known_attribute_sources_for_test(root.path());
    let scope = bootstrap(root.path(), storage.path());
    assert_eq!(base_kind(&scope), Some(BaseKind::Tree));
    let proof = read_tree_proof(&member_dir(storage.path(), &scope)).unwrap();
    assert_eq!(proof.receipts.len(), FILES.len());
    for (name, bytes) in FILES {
        let receipt = &proof.receipts[&rel(name)];
        assert_eq!(
            receipt.git_oid,
            crate::blob_store::v2::to_hex(
                &crate::git_index_reader::ObjectFormat::Sha1.blob_oid(bytes)
            )
        );
    }

    let (snapshot, work) = rebind(root.path(), storage.path());
    assert_eq!(work.tree_receipts, FILES.len(), "{work:?}");
    assert_eq!(work.reads, 0, "{work:?}");
    assert_current_bytes(&snapshot, root.path());

    // The receipts alone suffice: the worker's own evidence is not needed.
    remove_worker_evidence(storage.path());
    let (snapshot, work) = rebind(root.path(), storage.path());
    assert_eq!(work.tree_receipts, FILES.len(), "{work:?}");
    assert_eq!(work.reads, 0, "{work:?}");
    assert_current_bytes(&snapshot, root.path());
}

#[test]
fn a_membership_base_never_takes_the_receipt_fast_path() {
    let _serial = crate::blob_store::v3::serial_for_test();
    for commit in [false, true] {
        let root = checkout(commit);
        let storage = tempfile::tempdir().unwrap();
        let _attributes = known_attribute_sources_for_test(root.path());
        if commit {
            // A dirty tracked file: the index no longer proves the bytes.
            write_old(&root.path().join("file.rs"), b"fn edited() {}\n");
        }
        let scope = bootstrap(root.path(), storage.path());
        assert_eq!(base_kind(&scope), Some(BaseKind::Membership), "{commit}");
        assert!(!member_dir(storage.path(), &scope)
            .join(TREE_PROOF_FILE)
            .exists());

        let (_, work) = rebind(root.path(), storage.path());
        assert_eq!(work.tree_receipts, 0, "{work:?}");
        remove_worker_evidence(storage.path());
        let (snapshot, work) = rebind(root.path(), storage.path());
        assert_eq!(work.tree_receipts, 0, "{work:?}");
        assert_eq!(work.reads, FILES.len(), "full check: {work:?}");
        assert_current_bytes(&snapshot, root.path());
    }
}

#[test]
fn a_tree_base_without_a_holding_proof_falls_back_to_the_full_check() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let root = checkout(true);
    let storage = tempfile::tempdir().unwrap();
    let _attributes = known_attribute_sources_for_test(root.path());
    let scope = bootstrap(root.path(), storage.path());
    assert_eq!(base_kind(&scope), Some(BaseKind::Tree));
    remove_worker_evidence(storage.path());
    let proof_path = member_dir(storage.path(), &scope).join(TREE_PROOF_FILE);
    let original = std::fs::read(&proof_path).unwrap();
    let full_check = |label: &str| {
        let (snapshot, work) = rebind(root.path(), storage.path());
        assert_eq!(work.tree_receipts, 0, "{label}: {work:?}");
        assert_eq!(work.reads, FILES.len(), "{label}: {work:?}");
        assert_current_bytes(&snapshot, root.path());
    };

    // No proof file at all.
    std::fs::remove_file(&proof_path).unwrap();
    full_check("missing proof");

    // A receipt edited after the proof was written.
    std::fs::write(&proof_path, &original).unwrap();
    {
        let db = rusqlite::Connection::open(&proof_path).unwrap();
        let record: Vec<u8> = db
            .query_row(
                "SELECT record FROM blob_receipts WHERE path=?1",
                [b"lib.rs".as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        let mut receipt: BlobReceipt = serde_json::from_slice(&record).unwrap();
        receipt.observed_ns += 1;
        db.execute(
            "UPDATE blob_receipts SET record=?1 WHERE path=?2",
            rusqlite::params![serde_json::to_vec(&receipt).unwrap(), b"lib.rs".as_slice()],
        )
        .unwrap();
    }
    full_check("receipt digest mismatch");

    // A proof from a reader of another format.
    std::fs::write(&proof_path, &original).unwrap();
    rusqlite::Connection::open(&proof_path)
        .unwrap()
        .pragma_update(None, "user_version", 2)
        .unwrap();
    full_check("newer proof version");

    // An intact proof, but Git no longer proves the full index.
    std::fs::write(&proof_path, &original).unwrap();
    git(
        root.path(),
        &["update-index", "--assume-unchanged", "lib.rs"],
    );
    full_check("assume-unchanged entry");
    git(
        root.path(),
        &["update-index", "--no-assume-unchanged", "lib.rs"],
    );

    // Restoring everything restores the fast path.
    let (_, work) = rebind(root.path(), storage.path());
    assert_eq!(work.tree_receipts, FILES.len(), "{work:?}");
    assert_eq!(work.reads, 0, "{work:?}");
}

#[test]
fn a_file_whose_bytes_no_longer_match_its_receipt_takes_the_full_check() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let root = checkout(true);
    let storage = tempfile::tempdir().unwrap();
    let _attributes = known_attribute_sources_for_test(root.path());
    let scope = bootstrap(root.path(), storage.path());
    assert_eq!(base_kind(&scope), Some(BaseKind::Tree));
    remove_worker_evidence(storage.path());
    // Same length and the same mtime as the receipt recorded: only the change
    // time and the bytes differ, and the index still names the old blob.
    let edited = FILES[0].1.to_vec();
    let edited: Vec<u8> = edited
        .into_iter()
        .map(|b| if b == b'1' { b'2' } else { b })
        .collect();
    assert_ne!(edited, FILES[0].1);
    write_old(&root.path().join(FILES[0].0), &edited);

    let (snapshot, work) = rebind(root.path(), storage.path());
    assert_eq!(work.reads, 1, "{work:?}");
    assert_eq!(work.tree_receipts, 1, "{work:?}");
    assert_eq!(
        snapshot.disk_state(&rel(FILES[0].0)),
        DiskState::of_bytes(&edited)
    );
    assert_current_bytes(&snapshot, root.path());
}

#[test]
fn the_tree_proof_names_the_first_check_a_checkout_fails() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let route = crate::views::snapshot::V3Route {
        storage: storage.path().to_owned(),
        family: FAMILY.into(),
        scope: WORKER.into(),
    };
    let paths: Vec<RelPath> = FILES.iter().map(|(name, _)| rel(name)).collect();
    let producers = producers();
    let outcome = |root: &Path| {
        let input = super::TreeBaseInput {
            route: &route,
            root,
            paths: &paths,
            policy: policy(),
            producers: &producers,
            ignore_fingerprint: None,
        };
        let guard = crate::views::verification::BindGuard::capture(root, None).unwrap();
        super::prove(&input, &guard).map(|proven| proven.receipts.len())
    };
    let plain = checkout(false);
    let _plain_attributes = known_attribute_sources_for_test(plain.path());
    assert_eq!(outcome(plain.path()), Err("no_head_tree"));
    let root = checkout(true);
    let _attributes = known_attribute_sources_for_test(root.path());
    assert_eq!(outcome(root.path()), Ok(FILES.len()));
    let slot = std::sync::Mutex::new(None);
    let built = super::try_bootstrap(
        super::TreeBaseInput {
            route: &route,
            root: root.path(),
            paths: &paths,
            policy: policy(),
            producers: &producers,
            ignore_fingerprint: None,
        },
        &slot,
    );
    assert_eq!(
        built.as_ref().err(),
        None,
        "a clean checkout builds a tree-keyed base"
    );
    assert_eq!(built.unwrap().identity().kind(), BaseKind::Tree);
    slot.lock().unwrap().take().unwrap().release().unwrap();
    git(
        root.path(),
        &["update-index", "--assume-unchanged", "lib.rs"],
    );
    assert_eq!(outcome(root.path()), Err("index_tree_refused"));
    git(
        root.path(),
        &["update-index", "--no-assume-unchanged", "lib.rs"],
    );
    write_old(&root.path().join("lib.rs"), b"pub fn edited() {}\n");
    assert_eq!(outcome(root.path()), Err("member_differs_from_index_blob"));
    git(root.path(), &["add", "lib.rs"]);
    assert_eq!(outcome(root.path()), Err("index_tree_differs_from_head"));
}

#[test]
fn a_tree_proof_round_trips_and_refuses_another_application() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let dir = storage.path().join("member");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("source");
    std::fs::write(&file, b"bytes").unwrap();
    let receipt = BlobReceipt {
        version: 1,
        mode: 0o100644,
        size: 5,
        content: crate::blob_store::v2::ContentHash::of(b"bytes").to_hex(),
        git_oid: "1".repeat(40),
        stat: NativeStat::read(&file).unwrap(),
        observed_ns: 7,
        policy: "policy".into(),
        delivery_seq: 0,
    };
    let proof = TreeProof {
        base_scope: "base-t1-00".into(),
        identity_digest: "aa".into(),
        membership_digest: "bb".into(),
        object_format: "sha1".into(),
        head_tree: "2".repeat(40),
        root: FileIdentity::StrictOnly {
            volume: 1,
            index: 2,
            birth: None,
        },
        policy: "policy".into(),
        receipts: [(rel("a.rs"), receipt)].into_iter().collect(),
    };
    write_tree_proof(storage.path(), FAMILY, &dir, &proof).unwrap();
    assert_eq!(read_tree_proof(&dir), Some(proof));
    assert!(std::fs::read_dir(&dir).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".tmp.")));
    let db = rusqlite::Connection::open(dir.join(TREE_PROOF_FILE)).unwrap();
    db.pragma_update(None, "application_id", TREE_PROOF_APP + 1)
        .unwrap();
    drop(db);
    assert_eq!(read_tree_proof(&dir), None);
}
