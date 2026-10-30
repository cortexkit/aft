//! Tests of base identity, the worker insertion protocol and the foreign
//! reader. Fixture helpers are shared with the sweep and builder tests.

use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::blob_store::v2::{ContentHash, FamilyKey, FamilyPlane};
use crate::views::base_builder::{publish_base, BasePublication};
use crate::views::manifest_v2::{EntryPlanes, ManifestHeader, Producers, PublishedFrontier};
use crate::views::readiness::PlaneState;
use crate::views::RelPath;

pub(crate) const FAMILY: &str = "00112233445566ff";

pub(crate) fn producers() -> Producers {
    Producers {
        trigram: "tri".into(),
        semantic: Some("sem".into()),
        callgraph: "graph".into(),
    }
}

pub(crate) fn header() -> ManifestHeader {
    ManifestHeader {
        producers: producers(),
        head_tree: None,
        ignore_fingerprint: None,
        segment: None,
    }
}

/// The trigram key the fixtures use for `bytes`.
pub(crate) fn key_of(bytes: &[u8]) -> FamilyKey {
    FamilyKey::new(FamilyPlane::Trigram, *blake3::hash(bytes).as_bytes())
}

pub(crate) fn path(name: &str) -> RelPath {
    RelPath::new(name.as_bytes().to_vec()).unwrap()
}

/// A regular entry whose trigram plane is Ready under `key_of(bytes)` and
/// whose semantic plane is still pending.
pub(crate) fn entry(bytes: &[u8]) -> EntryV2 {
    EntryV2::regular(
        ContentHash::of(bytes),
        bytes.len() as u64,
        EntryPlanes {
            trigram: Some(PlaneState::ready(&key_of(bytes))),
            semantic: Some(PlaneState::pending("awaiting the base owner")),
            callgraph: None,
        },
    )
}

pub(crate) fn base_manifest(files: &[(&str, &[u8])]) -> ManifestV2 {
    let mut manifest = ManifestV2::new_v3(header(), None, PublishedFrontier::default());
    for (name, bytes) in files {
        manifest.insert(path(name), entry(bytes)).unwrap();
    }
    manifest
}

/// Puts each payload into the family's v3 trigram store.
pub(crate) fn put_rows(storage: &Path, payloads: &[&[u8]]) {
    v3::publish(storage, FAMILY, 1 << 20, None, |publication| {
        for payload in payloads {
            publication.put(&key_of(payload), payload, 1)?;
        }
        Ok(())
    })
    .unwrap();
}

pub(crate) fn plane_has(storage: &Path, payload: &[u8]) -> bool {
    v3::PlaneReader::open_existing(storage, FAMILY, FamilyPlane::Trigram)
        .unwrap()
        .get(&key_of(payload))
        .unwrap()
        .is_some()
}

/// Publishes a base over `files` (rows first) and returns its identity and
/// generation.
pub(crate) fn publish_test_base(storage: &Path, files: &[(&str, &[u8])]) -> (BaseIdentity, String) {
    put_rows(
        storage,
        &files.iter().map(|(_, bytes)| *bytes).collect::<Vec<_>>(),
    );
    let manifest = base_manifest(files);
    let identity = BaseIdentity::of(FAMILY, &manifest).unwrap();
    let BasePublication::Published { generation, .. } =
        publish_base(storage, FAMILY, &manifest, None).unwrap()
    else {
        panic!("first base publication lost")
    };
    (identity, generation)
}

pub(crate) fn frontier() -> PublishedFrontier {
    PublishedFrontier {
        delta_cut_version: 7,
        watcher_epoch: 3,
        published_at_ms: 1_000,
        verified_frontier: None,
    }
}

/// A worker delta over the base: one changed file and one deleted base file.
pub(crate) fn worker_delta(
    identity: &BaseIdentity,
    generation: &str,
    changed: &[u8],
) -> ManifestV2 {
    let mut delta = ManifestV2::new_v3(header(), Some(identity.header(generation)), frontier());
    delta
        .insert(path("src/changed.rs"), entry(changed))
        .unwrap();
    delta
}

pub(crate) fn bind(
    storage: &Path,
    scope: &str,
    identity: &BaseIdentity,
    generation: &str,
    delta: ManifestV2,
    previous: Option<String>,
) -> Result<WorkerOutcome> {
    let root = storage.join("roots").join(scope);
    std::fs::create_dir_all(&root).unwrap();
    let header = identity.header(generation);
    publish_worker(
        WorkerPublication {
            storage,
            family: FAMILY,
            worker_scope: scope,
            root: &root,
            base: &header,
            delta,
            previous,
        },
        None,
    )
}

/// `(worker, base, state, worker_generation)` of every referrer row.
pub(crate) fn refs(storage: &Path) -> Vec<(String, String, String, Option<String>)> {
    LedgerReader::open_existing(storage, FAMILY)
        .unwrap()
        .read(|c| {
            Ok(c.prepare("SELECT worker_scope,base_scope,state,worker_generation FROM base_refs ORDER BY worker_scope,base_scope,nonce")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap()
}

pub(crate) fn marker_files(dir: &Path) -> usize {
    let readers = dir.join("readers");
    let Ok(generations) = std::fs::read_dir(readers) else {
        return 0;
    };
    generations
        .flatten()
        .map(|generation| std::fs::read_dir(generation.path()).map_or(0, |files| files.count()))
        .sum()
}

pub(crate) fn published(outcome: WorkerOutcome) -> (String, Arc<ComposedGeneration>) {
    match outcome {
        WorkerOutcome::Published {
            generation,
            composed,
        } => (generation, composed),
        other => panic!("expected a publication, got {other:?}"),
    }
}

#[test]
fn membership_digest_ignores_readiness_and_binds_content_and_paths() {
    let a = base_manifest(&[("a.rs", b"one"), ("b.rs", b"two")]);
    let mut ready = a.clone();
    if let Some(EntryV2::Regular { planes, .. }) = ready.get_mut(&path("a.rs")) {
        planes.semantic = Some(PlaneState::ready(&FamilyKey::new(
            FamilyPlane::Semantic,
            [9; 32],
        )));
    }
    assert_eq!(
        membership_digest(&a).unwrap(),
        membership_digest(&ready).unwrap()
    );
    let edited = base_manifest(&[("a.rs", b"one!"), ("b.rs", b"two")]);
    assert_ne!(
        membership_digest(&a).unwrap(),
        membership_digest(&edited).unwrap()
    );
    let renamed = base_manifest(&[("a2.rs", b"one"), ("b.rs", b"two")]);
    assert_ne!(
        membership_digest(&a).unwrap(),
        membership_digest(&renamed).unwrap()
    );
    let identity = BaseIdentity::of(FAMILY, &a).unwrap();
    assert!(identity.scope.starts_with(BASE_SCOPE_PREFIX));
    assert_ne!(identity, BaseIdentity::of("ffeeddccbbaa9988", &a).unwrap());
    let mut other_producer = a.clone();
    other_producer.header_mut().producers.semantic = None;
    assert_ne!(identity, BaseIdentity::of(FAMILY, &other_producer).unwrap());
}

#[test]
fn a_membership_keyed_base_never_claims_tree_identity() {
    let mut tree = base_manifest(&[("a.rs", b"one")]);
    tree.header_mut().head_tree = Some("caller-supplied".into());
    assert!(matches!(
        membership_digest(&tree),
        Err(BaseRefError::Corrupt(_))
    ));
    let mut oid = base_manifest(&[("a.rs", b"one")]);
    if let Some(EntryV2::Regular { git_oid, .. }) = oid.get_mut(&path("a.rs")) {
        *git_oid = Some("0".repeat(40));
    }
    assert!(matches!(
        membership_digest(&oid),
        Err(BaseRefError::Corrupt(_))
    ));
}

/// A tree claim over `files`: every entry carries its real Git blob OID and
/// the header names `tree` (a well-formed but unproven string).
pub(crate) fn tree_manifest(files: &[(&str, &[u8])], tree: &str) -> ManifestV2 {
    let mut manifest = base_manifest(files);
    manifest.header_mut().head_tree = Some(tree.into());
    for (name, bytes) in files {
        if let Some(EntryV2::Regular { git_oid, .. }) = manifest.get_mut(&path(name)) {
            *git_oid = Some(to_hex(
                &crate::git_index_reader::ObjectFormat::Sha1.blob_oid(bytes),
            ));
        }
    }
    manifest
}

#[test]
fn tree_claims_get_their_own_identity_kind_and_never_a_membership_scope() {
    let files: &[(&str, &[u8])] = &[("a.rs", b"one"), ("b.rs", b"two")];
    let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let manifest = tree_manifest(files, tree);
    let identity = BaseIdentity::of(FAMILY, &manifest).unwrap();
    assert_eq!(identity.kind(), BaseKind::Tree);
    assert!(identity.scope.starts_with(TREE_BASE_SCOPE_PREFIX));
    let membership = BaseIdentity::of(FAMILY, &base_manifest(files)).unwrap();
    assert_eq!(membership.kind(), BaseKind::Membership);
    assert_ne!(identity.scope, membership.scope);
    assert_ne!(identity.membership, membership.membership);

    // The tree string and every OID are part of the identity.
    let other_tree = tree_manifest(files, &"1".repeat(40));
    assert_ne!(BaseIdentity::of(FAMILY, &other_tree).unwrap(), identity);
    let mut other_oid = manifest.clone();
    if let Some(EntryV2::Regular { git_oid, .. }) = other_oid.get_mut(&path("a.rs")) {
        *git_oid = Some("2".repeat(40));
    }
    assert_ne!(BaseIdentity::of(FAMILY, &other_oid).unwrap(), identity);
    // Readiness successors keep the identity.
    let mut ready = manifest.clone();
    if let Some(EntryV2::Regular { planes, .. }) = ready.get_mut(&path("a.rs")) {
        planes.semantic = Some(PlaneState::ready(&FamilyKey::new(
            FamilyPlane::Semantic,
            [9; 32],
        )));
    }
    assert_eq!(BaseIdentity::of(FAMILY, &ready).unwrap(), identity);

    // Malformed claims are refused, never reinterpreted.
    for forged in ["caller-supplied", &tree.to_uppercase(), &tree[..39]] {
        let mut bad = manifest.clone();
        bad.header_mut().head_tree = Some(forged.to_owned());
        assert!(matches!(
            BaseIdentity::of(FAMILY, &bad),
            Err(BaseRefError::Corrupt(_))
        ));
    }
    let mut missing_oid = manifest.clone();
    if let Some(EntryV2::Regular { git_oid, .. }) = missing_oid.get_mut(&path("b.rs")) {
        *git_oid = None;
    }
    assert!(matches!(
        BaseIdentity::of(FAMILY, &missing_oid),
        Err(BaseRefError::Corrupt(_))
    ));
    let mut sha256_oid = manifest.clone();
    if let Some(EntryV2::Regular { git_oid, .. }) = sha256_oid.get_mut(&path("b.rs")) {
        *git_oid = Some("3".repeat(64));
    }
    assert!(matches!(
        BaseIdentity::of(FAMILY, &sha256_oid),
        Err(BaseRefError::Corrupt(_))
    ));
    // Dropping the tree while keeping the OIDs does not relabel the claim as
    // membership-keyed.
    let mut relabelled = manifest.clone();
    relabelled.header_mut().head_tree = None;
    assert!(matches!(
        BaseIdentity::of(FAMILY, &relabelled),
        Err(BaseRefError::Corrupt(_))
    ));
}

/// A reader that knows only membership-keyed bases (the rules this module had
/// before tree-keyed bases): it accepts a scope only with the membership
/// prefix, and only a manifest whose membership digest derives that scope.
fn membership_only_reader_accepts(manifest: &ManifestV2, scope: &str) -> bool {
    scope.starts_with(BASE_SCOPE_PREFIX)
        && membership_digest(manifest).is_ok_and(|membership| {
            base_scope_for(&identity_digest(FAMILY, manifest.header(), &membership)) == scope
        })
}

#[test]
fn an_older_membership_only_reader_refuses_a_published_tree_keyed_base() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let files: &[(&str, &[u8])] = &[("a.rs", b"alpha")];
    put_rows(storage.path(), &[b"alpha"]);
    let manifest = tree_manifest(files, "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    let identity = BaseIdentity::of(FAMILY, &manifest).unwrap();
    let BasePublication::Published { generation, .. } =
        publish_base(storage.path(), FAMILY, &manifest, None).unwrap()
    else {
        panic!("first tree base publication lost")
    };
    // The current resolver authenticates the scope with its tree identity.
    let resolved = resolve_base(storage.path(), FAMILY, &identity.scope).unwrap();
    assert_eq!(resolved.identity().kind(), BaseKind::Tree);
    assert_eq!(resolved.generation(), generation);
    // The durable manifest, as an older reader would load it, is refused
    // under both of its rules: the scope prefix and the membership digest.
    let stored = ViewStore::existing_dir(resolved.view_dir().to_path_buf())
        .unwrap()
        .load_manifest_v2(&generation)
        .unwrap();
    assert!(!membership_only_reader_accepts(&stored, &identity.scope));
    assert!(matches!(
        membership_digest(&stored),
        Err(BaseRefError::Corrupt(_))
    ));
    // No membership-keyed scope can name the tree-keyed manifest either.
    let membership = BaseIdentity::of(FAMILY, &base_manifest(files)).unwrap();
    assert!(!membership_only_reader_accepts(&stored, &membership.scope));
    // A worker cannot register under a tree-keyed base scope.
    assert!(matches!(
        with_base_barrier(storage.path(), FAMILY, |tx| register_view_in(
            tx,
            storage.path(),
            FAMILY,
            &identity.scope,
            Path::new("/checkout")
        )),
        Err(BaseRefError::Corrupt(_))
    ));
}

#[test]
fn resolution_authenticates_the_pinned_base_and_rejects_forged_headers() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("a.rs", b"alpha")]);
    let base = resolve_base(storage.path(), FAMILY, &identity.scope).unwrap();
    assert_eq!(base.generation(), generation);
    assert_eq!(base.identity(), &identity);
    assert_eq!(marker_files(base.view_dir()), 1);
    base.check_header(&identity.header("any generation: diagnostic only"))
        .unwrap();
    let mut forged = identity.header(&generation);
    forged.membership_digest = "11".repeat(32);
    assert!(matches!(
        base.check_header(&forged),
        Err(BaseRefError::Corrupt(_))
    ));
    let mut forged = identity.header(&generation);
    forged.identity_digest = "22".repeat(32);
    assert!(matches!(
        base.check_header(&forged),
        Err(BaseRefError::Corrupt(_))
    ));
    let dir = base.view_dir().to_path_buf();
    drop(base);
    assert_eq!(marker_files(&dir), 0);
    assert!(matches!(
        resolve_base(storage.path(), FAMILY, &base_scope_for(&[3; 32])),
        Err(BaseRefError::Unavailable(_))
    ));
}

#[test]
fn a_manifest_stored_under_another_identity_is_never_authenticated() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (_identity, _) = publish_test_base(storage.path(), &[("a.rs", b"alpha")]);
    // Register a base scope and give it a manifest whose identity derives a
    // different scope: the stored strings are not proof.
    let scope = base_scope_for(&[5; 32]);
    let dir = with_base_barrier(storage.path(), FAMILY, |tx| {
        Ok(crate::views::base_builder::register_base_in(tx, storage.path(), FAMILY, &scope)?.0)
    })
    .unwrap();
    let store = ViewStore::open_dir(dir).unwrap();
    let manifest = base_manifest(&[("a.rs", b"beta")]);
    let name = GenerationName::for_manifest(&manifest).unwrap();
    let prepared = store.prepare_v2(&name, None, &manifest, None).unwrap();
    assert_eq!(
        store.commit_v2(prepared, None).unwrap(),
        PublishV2::Published
    );
    assert!(matches!(
        resolve_base(storage.path(), FAMILY, &scope),
        Err(BaseRefError::Corrupt(_))
    ));
}

#[test]
fn a_worker_publication_records_a_published_referrer_and_composes_over_the_base() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(
        storage.path(),
        &[("src/a.rs", b"alpha"), ("src/changed.rs", b"old")],
    );
    put_rows(storage.path(), &[b"new"]);
    let (worker_generation, composed) = published(
        bind(
            storage.path(),
            "worker-a",
            &identity,
            &generation,
            worker_delta(&identity, &generation, b"new"),
            None,
        )
        .unwrap(),
    );
    assert_eq!(
        refs(storage.path()),
        vec![(
            "worker-a".to_string(),
            identity.scope.clone(),
            REF_PUBLISHED.to_string(),
            Some(worker_generation.clone())
        )]
    );
    // Lookups: the delta's changed entry, the inherited base entry.
    assert_eq!(
        composed.entry(&path("src/changed.rs")),
        Some(&entry(b"new"))
    );
    assert_eq!(composed.entry(&path("src/a.rs")), Some(&entry(b"alpha")));
    assert_eq!(composed.published(), Some(&frontier()));
    // The bundle owns one marker on the base generation and one on its own;
    // the insertion marker is gone.
    let base_dir = v3::view_dir(storage.path(), FAMILY, &identity.scope).unwrap();
    let worker_dir = v3::view_dir(storage.path(), FAMILY, "worker-a").unwrap();
    assert_eq!(marker_files(&base_dir), 1);
    assert_eq!(marker_files(&worker_dir), 1);
    drop(composed);
    assert_eq!(marker_files(&base_dir), 0);
    assert_eq!(marker_files(&worker_dir), 0);
    // Nothing under the unassigned namespace.
    assert!(!storage.path().join("views/v3").exists());
}

#[test]
fn a_lost_cas_deletes_only_this_jobs_intent_row() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("src/changed.rs", b"old")]);
    put_rows(storage.path(), &[b"one", b"two"]);
    let (first, _composed) = published(
        bind(
            storage.path(),
            "worker-a",
            &identity,
            &generation,
            worker_delta(&identity, &generation, b"one"),
            None,
        )
        .unwrap(),
    );
    // A stale Wold: the CAS must lose, and only the losing job's row goes.
    let outcome = bind(
        storage.path(),
        "worker-a",
        &identity,
        &generation,
        worker_delta(&identity, &generation, b"two"),
        None,
    )
    .unwrap();
    assert!(
        matches!(&outcome, WorkerOutcome::Lost { current: Some(current), equivalent: false } if *current == first),
        "{outcome:?}"
    );
    assert_eq!(
        refs(storage.path()),
        vec![(
            "worker-a".into(),
            identity.scope.clone(),
            REF_PUBLISHED.into(),
            Some(first)
        )]
    );
}

#[test]
fn a_failed_verify_cleans_up_its_intent_before_any_pointer_moves() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("src/changed.rs", b"old")]);
    let mut forged = identity.clone();
    forged.membership = [1; 32];
    let error = bind(
        storage.path(),
        "worker-a",
        &forged,
        &generation,
        worker_delta(&identity, &generation, b"x"),
        None,
    )
    .unwrap_err();
    assert!(matches!(error, BaseRefError::Corrupt(_)), "{error}");
    assert!(refs(storage.path()).is_empty());
    let worker = v3::view_dir(storage.path(), FAMILY, "worker-a").unwrap();
    let pointer =
        ViewStore::existing_dir(worker).map(|store| store.current_generation_read_only().unwrap());
    assert!(matches!(pointer, None | Some(None)), "{pointer:?}");
    let base_dir = v3::view_dir(storage.path(), FAMILY, &identity.scope).unwrap();
    assert_eq!(marker_files(&base_dir), 0);
}

#[test]
fn final_checkout_validation_runs_after_marker_validation_and_can_refuse_cas() {
    let _serial = v3::serial_for_test();
    for remove_base_before_validation in [false, true] {
        let storage = tempfile::tempdir().unwrap();
        let (identity, generation) =
            publish_test_base(storage.path(), &[("src/changed.rs", b"old")]);
        let header = identity.header(&generation);
        let root = storage.path().join("worker-root");
        std::fs::create_dir(&root).unwrap();
        let finished = std::cell::Cell::new(false);
        let validated = std::cell::Cell::new(false);
        let result = publish_worker_with(
            WorkerPublication {
                storage: storage.path(),
                family: FAMILY,
                worker_scope: "worker-a",
                root: &root,
                base: &header,
                delta: worker_delta(&identity, &generation, b"new"),
                previous: None,
            },
            None,
            &mut |_| {
                finished.set(true);
                if remove_base_before_validation {
                    with_base_barrier(storage.path(), FAMILY, |tx| {
                        tx.execute(
                            "UPDATE members SET state='evicting' WHERE scope=?1",
                            [&identity.scope],
                        )?;
                        Ok(())
                    })?;
                }
                Ok(())
            },
            &mut || {
                assert!(finished.get());
                validated.set(true);
                Err(BaseRefError::Unavailable(
                    "HEAD changed after admission".into(),
                ))
            },
        );
        assert!(result.is_err());
        assert_eq!(validated.get(), !remove_base_before_validation);
        assert!(refs(storage.path()).is_empty());
        let worker = v3::view_dir(storage.path(), FAMILY, "worker-a").unwrap();
        assert!(ViewStore::existing_dir(worker)
            .unwrap()
            .current_generation_read_only()
            .unwrap()
            .is_none());
    }
}

#[test]
fn a_landed_cas_is_published_and_never_erased_as_abort_cleanup() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("src/changed.rs", b"old")]);
    put_rows(storage.path(), &[b"landed"]);
    AFTER_WORKER_CAS.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| Some(io::Error::other("sync failed after CAS"))))
    });
    let result = bind(
        storage.path(),
        "worker-a",
        &identity,
        &generation,
        worker_delta(&identity, &generation, b"landed"),
        None,
    );
    AFTER_WORKER_CAS.with(|hook| hook.borrow_mut().take());
    assert!(result.is_err());
    let worker = v3::view_dir(storage.path(), FAMILY, "worker-a").unwrap();
    let current = ViewStore::existing_dir(worker)
        .unwrap()
        .current_generation_read_only()
        .unwrap()
        .unwrap();
    assert_eq!(
        refs(storage.path()),
        vec![(
            "worker-a".into(),
            identity.scope.clone(),
            REF_PUBLISHED.into(),
            Some(current)
        )]
    );
    for _ in 0..3 {
        let report = crate::views::base_sweep::sweep_family(storage.path(), FAMILY, None).unwrap();
        assert!(report.reclaimed_bases.is_empty(), "{report:?}");
    }
    let reader = open_foreign_v3(storage.path(), FAMILY, "worker-a", &producers())
        .unwrap()
        .unwrap();
    assert_eq!(
        reader.entry(&path("src/changed.rs")),
        Some(&entry(b"landed"))
    );
    assert!(plane_has(storage.path(), b"old"));
}

#[test]
fn a_foreign_reader_owns_both_markers_and_reports_the_published_frontier() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("src/a.rs", b"alpha")]);
    put_rows(storage.path(), &[b"new"]);
    drop(
        bind(
            storage.path(),
            "worker-a",
            &identity,
            &generation,
            worker_delta(&identity, &generation, b"new"),
            None,
        )
        .unwrap(),
    );
    let reader = crate::views::read::open_foreign_v3_generation(
        storage.path(),
        FAMILY,
        "worker-a",
        &producers(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(reader.published(), Some(&frontier()));
    assert_eq!(reader.entry(&path("src/a.rs")), Some(&entry(b"alpha")));
    let base_dir = v3::view_dir(storage.path(), FAMILY, &identity.scope).unwrap();
    let worker_dir = v3::view_dir(storage.path(), FAMILY, "worker-a").unwrap();
    assert_eq!(marker_files(&base_dir), 1);
    assert_eq!(marker_files(&worker_dir), 1);
    // The base cannot be reclaimed while the reader holds it, even after the
    // worker's referrer row is gone.
    with_base_barrier(storage.path(), FAMILY, |tx| {
        tx.execute("DELETE FROM base_refs", [])?;
        Ok(())
    })
    .unwrap();
    for _ in 0..3 {
        let report = crate::views::base_sweep::sweep_family(storage.path(), FAMILY, None).unwrap();
        assert!(report.reclaimed_bases.is_empty(), "{report:?}");
    }
    assert_eq!(reader.entry(&path("src/a.rs")), Some(&entry(b"alpha")));
    drop(reader);
    assert_eq!(marker_files(&base_dir), 0);
    assert_eq!(marker_files(&worker_dir), 0);
}

#[test]
fn a_foreign_reader_reports_a_missing_base_unavailable_after_its_retries() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("src/a.rs", b"alpha")]);
    drop(
        bind(
            storage.path(),
            "worker-a",
            &identity,
            &generation,
            worker_delta(&identity, &generation, b"alpha"),
            None,
        )
        .unwrap(),
    );
    // Remove the base behind the protocol's back (a damaged store).
    with_base_barrier(storage.path(), FAMILY, |tx| {
        tx.execute("DELETE FROM members WHERE kind='base'", [])?;
        Ok(())
    })
    .unwrap();
    let error = open_foreign_v3(storage.path(), FAMILY, "worker-a", &producers()).unwrap_err();
    assert!(matches!(error, BaseRefError::Unavailable(_)), "{error}");
    let worker_dir = v3::view_dir(storage.path(), FAMILY, "worker-a").unwrap();
    assert_eq!(
        marker_files(&worker_dir),
        0,
        "every attempt dropped its marker"
    );
}

#[test]
fn the_first_base_publication_needs_a_complete_lexical_plane() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let mut manifest = base_manifest(&[("a.rs", b"alpha")]);
    if let Some(EntryV2::Regular { planes, .. }) = manifest.get_mut(&path("a.rs")) {
        planes.trigram = Some(PlaneState::pending("not tokenized"));
    }
    assert!(matches!(
        publish_base(storage.path(), FAMILY, &manifest, None),
        Err(BaseRefError::Corrupt(_))
    ));
}

#[test]
fn readiness_successors_are_monotone_and_keep_identity_and_segment() {
    use crate::views::base_builder::check_successor;
    let base = base_manifest(&[("a.rs", b"alpha")]);
    let set = |state: PlaneState| {
        let mut next = base.clone();
        if let Some(EntryV2::Regular { planes, .. }) = next.get_mut(&path("a.rs")) {
            planes.semantic = Some(state);
        }
        next
    };
    let ready = set(PlaneState::ready(&FamilyKey::new(
        FamilyPlane::Semantic,
        [4; 32],
    )));
    let failed = set(PlaneState::Failed {
        reason: "model outage".into(),
        producer: "sem".into(),
    });
    check_successor(FAMILY, &base, &ready).unwrap();
    check_successor(FAMILY, &base, &failed).unwrap();
    check_successor(FAMILY, &failed, &ready).unwrap();
    assert!(
        check_successor(FAMILY, &ready, &base).is_err(),
        "Ready -> Pending"
    );
    assert!(
        check_successor(FAMILY, &ready, &failed).is_err(),
        "Ready -> Failed"
    );
    let other_ready = set(PlaneState::ready(&FamilyKey::new(
        FamilyPlane::Semantic,
        [5; 32],
    )));
    assert!(
        check_successor(FAMILY, &ready, &other_ready).is_err(),
        "a Ready key changed"
    );
    let mut segment = ready.clone();
    segment.header_mut().segment = Some("ab".repeat(32));
    assert!(
        check_successor(FAMILY, &ready, &segment).is_err(),
        "segment replaced"
    );
    let edited = base_manifest(&[("a.rs", b"alpha!")]);
    assert!(
        check_successor(FAMILY, &base, &edited).is_err(),
        "membership changed"
    );
}

#[test]
fn an_injected_failure_at_each_publication_boundary_keeps_references_safe() {
    use crate::views::base_sweep::sweep_family;
    use crate::views::contracts::{inject_fault_at, InjectedFault};
    for fault in InjectedFault::ALL {
        // BaseRegistered: the member row exists without a pointer. With no
        // live claim it is an empty, unreferenced base and goes after two
        // sweeps.
        {
            let _serial = v3::serial_for_test();
            let storage = tempfile::tempdir().unwrap();
            put_rows(storage.path(), &[b"alpha"]);
            let manifest = base_manifest(&[("a.rs", b"alpha")]);
            let identity = BaseIdentity::of(FAMILY, &manifest).unwrap();
            inject_fault_at(DurabilityStep::BaseRegistered, fault);
            assert!(publish_base(storage.path(), FAMILY, &manifest, None).is_err());
            assert!(matches!(
                resolve_base(storage.path(), FAMILY, &identity.scope),
                Err(BaseRefError::Unavailable(_))
            ));
            assert_eq!(
                sweep_family(storage.path(), FAMILY, None)
                    .unwrap()
                    .counted_bases
                    .len(),
                1
            );
            assert_eq!(
                sweep_family(storage.path(), FAMILY, None)
                    .unwrap()
                    .reclaimed_bases
                    .len(),
                1
            );
        }
        // BaseSuccessorPublished: the pointer already names the successor.
        {
            let _serial = v3::serial_for_test();
            let storage = tempfile::tempdir().unwrap();
            let (identity, first) = publish_test_base(storage.path(), &[("a.rs", b"alpha")]);
            let mut next = base_manifest(&[("a.rs", b"alpha")]);
            if let Some(EntryV2::Regular { planes, .. }) = next.get_mut(&path("a.rs")) {
                planes.semantic = Some(PlaneState::Failed {
                    reason: "model outage".into(),
                    producer: "sem".into(),
                });
            }
            inject_fault_at(DurabilityStep::BaseSuccessorPublished, fault);
            assert!(publish_base(storage.path(), FAMILY, &next, None).is_err());
            let base = resolve_base(storage.path(), FAMILY, &identity.scope).unwrap();
            assert_ne!(base.generation(), first);
            assert_eq!(base.manifest(), &next);
        }
        // BaseRefMarked: the job aborts and cleans up its own intent.
        // ReferrerPublished: the row is already published and stays.
        for step in [
            DurabilityStep::BaseRefMarked,
            DurabilityStep::ReferrerPublished,
        ] {
            let _serial = v3::serial_for_test();
            let storage = tempfile::tempdir().unwrap();
            let (identity, generation) =
                publish_test_base(storage.path(), &[("src/a.rs", b"alpha")]);
            inject_fault_at(step, fault);
            assert!(bind(
                storage.path(),
                "worker-a",
                &identity,
                &generation,
                worker_delta(&identity, &generation, b"alpha"),
                None,
            )
            .is_err());
            let base_dir = v3::view_dir(storage.path(), FAMILY, &identity.scope).unwrap();
            assert_eq!(marker_files(&base_dir), 0, "{step:?} {fault:?}");
            let rows = refs(storage.path());
            if step == DurabilityStep::BaseRefMarked {
                assert!(rows.is_empty(), "{step:?} {fault:?}: {rows:?}");
            } else {
                assert_eq!(rows.len(), 1, "{step:?} {fault:?}: {rows:?}");
                assert_eq!(rows[0].2, REF_PUBLISHED);
                for _ in 0..2 {
                    let report = sweep_family(storage.path(), FAMILY, None).unwrap();
                    assert!(report.counted_bases.is_empty(), "{report:?}");
                }
                assert!(
                    open_foreign_v3(storage.path(), FAMILY, "worker-a", &producers())
                        .unwrap()
                        .is_some()
                );
            }
        }
    }
}
