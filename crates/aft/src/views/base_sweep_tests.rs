//! Safety tests of the v3 base-reference sweep: referenced bases survive,
//! unreferenced bases go after two sweeps, dangling edges never abort, dead
//! owners' rows are retired or repaired, rows do not grow with
//! republication, and plane rows follow the marked closure.

use std::path::Path;

use super::*;
use crate::views::base_refs::tests::{
    base_manifest, bind, entry, frontier, header, key_of, path, plane_has, publish_test_base,
    published, put_rows, refs, worker_delta, FAMILY,
};
use crate::views::base_refs::{with_base_barrier, BaseIdentity, WorkerOutcome};
use crate::views::manifest_v2::{GenerationName, PublishV2};

fn sweep(storage: &Path) -> V3SweepReport {
    sweep_family(storage, FAMILY, None).unwrap()
}

fn base_dir(storage: &Path, identity: &BaseIdentity) -> PathBuf {
    v3::view_dir(storage, FAMILY, &identity.scope).unwrap()
}

fn base_registered(storage: &Path, identity: &BaseIdentity) -> bool {
    v3::LedgerReader::open_existing(storage, FAMILY)
        .unwrap()
        .member(&identity.scope)
        .unwrap()
        .is_some()
}

/// Publishes a self-contained v3 generation for `scope`, dropping its base
/// reference (a worker that rebuilt on the strict path).
fn republish_without_base(storage: &Path, scope: &str, previous: &str) -> String {
    let dir = v3::view_dir(storage, FAMILY, scope).unwrap();
    let store = ViewStore::open_dir(dir).unwrap();
    let manifest = base_manifest(&[("own.rs", b"own")]);
    put_rows(storage, &[b"own"]);
    let name = GenerationName::for_manifest(&manifest).unwrap();
    let prepared = store
        .prepare_v2(&name, Some(previous), &manifest, None)
        .unwrap();
    assert_eq!(
        store.commit_v2(prepared, None).unwrap(),
        PublishV2::Published
    );
    crate::views::wait_for_generation_maintenance_for_test(store.view_dir());
    name.to_string()
}

#[test]
fn a_referenced_base_survives_every_sweep_until_its_last_reference_goes() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("src/a.rs", b"alpha")]);
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
    drop(composed);
    for _ in 0..4 {
        let report = sweep(storage.path());
        assert!(report.reclaimed_bases.is_empty(), "{report:?}");
        assert!(report.counted_bases.is_empty(), "{report:?}");
        assert_eq!(report.valid_edges, 1, "{report:?}");
        assert!(plane_has(storage.path(), b"alpha"));
        assert!(plane_has(storage.path(), b"new"));
    }
    // The worker rebuilds without the base: one sweep counts, the next
    // reclaims, and never in a single sweep.
    republish_without_base(storage.path(), "worker-a", &worker_generation);
    let first = sweep(storage.path());
    assert_eq!(
        first.counted_bases,
        vec![identity.scope.clone()],
        "{first:?}"
    );
    assert!(first.reclaimed_bases.is_empty());
    assert!(base_registered(storage.path(), &identity));
    assert!(base_dir(storage.path(), &identity).exists());
    let second = sweep(storage.path());
    assert_eq!(
        second.reclaimed_bases,
        vec![identity.scope.clone()],
        "{second:?}"
    );
    assert!(!base_registered(storage.path(), &identity));
    assert!(!base_dir(storage.path(), &identity).exists());
    assert!(refs(storage.path()).is_empty());
    // Its rows went with the plane sweep of the next pass; the worker's own
    // rows stay.
    sweep(storage.path());
    assert!(!plane_has(storage.path(), b"alpha"));
    assert!(plane_has(storage.path(), b"own"));
    assert!(!storage.path().join("views/v3").exists());
}

#[test]
fn an_unreferenced_base_is_reclaimed_after_two_consecutive_sweeps_and_a_hold_resets_the_count() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("a.rs", b"alpha")]);
    assert_eq!(
        sweep(storage.path()).counted_bases,
        vec![identity.scope.clone()]
    );
    // A reader appears between the sweeps: the count resets.
    let marker =
        crate::root_cache::ReadMarker::create(&base_dir(storage.path(), &identity), &generation)
            .unwrap();
    let held = sweep(storage.path());
    assert!(
        held.reclaimed_bases.is_empty() && held.counted_bases.is_empty(),
        "{held:?}"
    );
    drop(marker);
    assert_eq!(
        sweep(storage.path()).counted_bases,
        vec![identity.scope.clone()]
    );
    assert!(base_registered(storage.path(), &identity));
    assert_eq!(
        sweep(storage.path()).reclaimed_bases,
        vec![identity.scope.clone()]
    );
}

#[test]
fn dangling_edges_are_counted_and_recorded_and_the_sweep_continues() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("a.rs", b"alpha")]);
    put_rows(storage.path(), &[b"garbage"]);
    // A worker whose manifest names a base that does not exist.
    let ghost = BaseIdentity {
        scope: crate::views::base_refs::base_scope_for(&[8; 32]),
        membership: [8; 32],
        identity: [8; 32],
    };
    let dir = with_base_barrier(storage.path(), FAMILY, |tx| {
        crate::views::base_refs::register_view_in(
            tx,
            storage.path(),
            FAMILY,
            "dangling",
            storage.path(),
        )
    })
    .unwrap();
    let store = ViewStore::open_dir(dir).unwrap();
    let delta = worker_delta(&ghost, "g", b"alpha");
    let name = GenerationName::for_manifest(&delta).unwrap();
    let prepared = store.prepare_v2(&name, None, &delta, None).unwrap();
    store.commit_v2(prepared, None).unwrap();
    let report = sweep(storage.path());
    assert_eq!(report.dangling_base_edges, 1, "{report:?}");
    assert_eq!(report.valid_edges, 0);
    // The rest of the sweep ran: the unreferenced base was counted and the
    // garbage row deleted, while the dangling worker's own key survived.
    assert_eq!(report.counted_bases, vec![identity.scope.clone()]);
    assert!(!plane_has(storage.path(), b"garbage"));
    assert!(plane_has(storage.path(), b"alpha"));
    let recorded: Vec<(String, String, String)> =
        v3::LedgerReader::open_existing(storage.path(), FAMILY)
            .unwrap()
            .read(|c| {
                Ok(
                    c.prepare("SELECT worker_scope,worker_generation,reason FROM strict_rebuild")?
                        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                )
            })
            .unwrap();
    assert_eq!(
        recorded,
        vec![(
            "dangling".into(),
            name.to_string(),
            "base_not_registered".into()
        )]
    );
    // A dangling observation never creates a hold.
    assert!(refs(storage.path()).is_empty());
    let _ = generation;
}

/// Inserts an `intent` row owned by a process that is certainly gone.
fn dead_intent(storage: &Path, worker: &str, identity: &BaseIdentity) {
    with_base_barrier(storage, FAMILY, |tx| {
        tx.execute(
            "INSERT INTO base_refs VALUES(?1,?2,'dead-nonce',?3,NULL,'intent',?4,1,?5)",
            params![
                worker,
                identity.scope,
                crate::blob_store::v2::to_hex(&identity.membership),
                i32::MAX as u32 - 7,
                gc_epoch(tx)?
            ],
        )?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_dead_owners_intent_is_orphaned_then_retired_and_holds_the_base_meanwhile() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, _generation) = publish_test_base(storage.path(), &[("a.rs", b"alpha")]);
    dead_intent(storage.path(), "gone-worker", &identity);
    let first = sweep(storage.path());
    assert_eq!(first.orphaned_rows, 1, "{first:?}");
    assert!(
        first.counted_bases.is_empty(),
        "the orphaned row still holds: {first:?}"
    );
    assert_eq!(refs(storage.path())[0].2, REF_ORPHANED);
    let second = sweep(storage.path());
    assert_eq!(second.deleted_rows, 1, "{second:?}");
    assert_eq!(second.counted_bases, vec![identity.scope.clone()]);
    assert!(base_registered(storage.path(), &identity));
    assert_eq!(
        sweep(storage.path()).reclaimed_bases,
        vec![identity.scope.clone()]
    );
}

#[test]
fn an_owner_that_died_between_its_cas_and_referrer_published_is_repaired() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("a.rs", b"alpha")]);
    // The pointer landed with the base header, but the row is still the dead
    // owner's intent.
    let dir = with_base_barrier(storage.path(), FAMILY, |tx| {
        crate::views::base_refs::register_view_in(
            tx,
            storage.path(),
            FAMILY,
            "worker-d",
            storage.path(),
        )
    })
    .unwrap();
    let store = ViewStore::open_dir(dir).unwrap();
    let delta = worker_delta(&identity, &generation, b"alpha");
    let name = GenerationName::for_manifest(&delta).unwrap();
    let prepared = store.prepare_v2(&name, None, &delta, None).unwrap();
    store.commit_v2(prepared, None).unwrap();
    dead_intent(storage.path(), "worker-d", &identity);
    for _ in 0..3 {
        let report = sweep(storage.path());
        assert!(
            report.reclaimed_bases.is_empty() && report.counted_bases.is_empty(),
            "{report:?}"
        );
    }
    assert_eq!(
        refs(storage.path()),
        vec![(
            "worker-d".into(),
            identity.scope.clone(),
            REF_PUBLISHED.into(),
            Some(name.to_string())
        )]
    );
}

#[test]
fn repeated_same_base_republications_do_not_grow_the_rows() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("src/a.rs", b"alpha")]);
    let mut previous = None;
    for round in 0..6u8 {
        let bytes = [b'v', round];
        put_rows(storage.path(), &[&bytes]);
        let (worker_generation, composed) = published(
            bind(
                storage.path(),
                "worker-a",
                &identity,
                &generation,
                worker_delta(&identity, &generation, &bytes),
                previous.clone(),
            )
            .unwrap(),
        );
        drop(composed);
        previous = Some(worker_generation);
    }
    crate::views::wait_for_generation_maintenance_for_test(
        &v3::view_dir(storage.path(), FAMILY, "worker-a").unwrap(),
    );
    assert_eq!(refs(storage.path()).len(), 6);
    // Only the row cap may cut this pass; a slow machine must not.
    let lock = ReservationLock::acquire(storage.path()).unwrap();
    let bounds = SweepBounds {
        reconcile_time: Duration::from_secs(60),
        ..SweepBounds::default()
    };
    let report = sweep_family_locked(storage.path(), FAMILY, &lock, None, bounds, None).unwrap();
    drop(lock);
    assert!(report.reconcile_complete, "{report:?}");
    let rows = refs(storage.path());
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].3, previous);
}

#[test]
fn reconciliation_stays_within_its_row_cap_and_resumes_on_the_next_pass() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, _generation) = publish_test_base(storage.path(), &[("a.rs", b"alpha")]);
    with_base_barrier(storage.path(), FAMILY, |tx| {
        for index in 0..10 {
            tx.execute(
                "INSERT INTO base_refs VALUES(?1,?2,?3,'00','g-superseded','published',1,1,0)",
                params![
                    format!("worker-{index}"),
                    identity.scope,
                    format!("n{index}")
                ],
            )?;
        }
        Ok(())
    })
    .unwrap();
    let lock = ReservationLock::acquire(storage.path()).unwrap();
    let bounds = SweepBounds {
        reconcile_rows: 4,
        // Only the row cap may cut these passes.
        reconcile_time: Duration::from_secs(60),
        ..SweepBounds::default()
    };
    let mut passes = 0;
    loop {
        let report =
            sweep_family_locked(storage.path(), FAMILY, &lock, None, bounds, None).unwrap();
        passes += 1;
        assert!(report.reconciled_rows <= 4, "{report:?}");
        if refs(storage.path()).is_empty() {
            // Until the last stale row went, the base stayed held.
            assert!(report.counted_bases.is_empty() || report.reconcile_complete);
            break;
        }
        assert!(
            report.counted_bases.is_empty(),
            "held while rows remain: {report:?}"
        );
        assert!(passes < 10);
    }
    assert_eq!(passes, 3);
}

#[test]
fn the_plane_row_sweep_reclaims_unreferenced_rows_and_keeps_referenced_ones() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (_identity, _generation) = publish_test_base(storage.path(), &[("a.rs", b"referenced")]);
    put_rows(storage.path(), &[b"unreferenced"]);
    let report = sweep(storage.path());
    assert!(report.plane_rows_complete, "{report:?}");
    assert_eq!(report.plane_rows_deleted, 1, "{report:?}");
    assert!(plane_has(storage.path(), b"referenced"));
    assert!(!plane_has(storage.path(), b"unreferenced"));
}

#[test]
fn override_ready_key_survives_fold_and_gc() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("src/a.rs", b"alpha")]);
    // A checkout-specific fill of an inherited path: its key lives only in
    // the worker's live pin until a published generation protects it.
    let fill = crate::blob_store::v2::FamilyKey::new(
        crate::blob_store::v2::FamilyPlane::Semantic,
        [42; 32],
    );
    let (first, composed) = published(
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
    drop(composed);
    let worker_dir = v3::view_dir(storage.path(), FAMILY, "worker-a").unwrap();
    let mut live = crate::pins::LivePin::create_v3(storage.path(), FAMILY, "worker-a").unwrap();
    live.protect(&[fill]).unwrap();
    v3::publish(storage.path(), FAMILY, 1 << 20, None, |publication| {
        publication.put(&fill, b"fill", 1)
    })
    .unwrap();
    let fill_present = || {
        v3::PlaneReader::open_existing(
            storage.path(),
            FAMILY,
            crate::blob_store::v2::FamilyPlane::Semantic,
        )
        .unwrap()
        .get(&fill)
        .unwrap()
        .is_some()
    };
    sweep(storage.path());
    assert!(fill_present(), "the live pin protects the unpublished fill");
    // Fold: the next worker generation carries the override.
    let mut delta = worker_delta(&identity, &generation, b"alpha");
    delta
        .set_readiness_override(
            path("src/a.rs"),
            crate::views::manifest_v2::EntryPlanes {
                semantic: Some(crate::views::readiness::PlaneState::ready(&fill)),
                ..Default::default()
            },
        )
        .unwrap();
    let (_second, composed) = published(
        bind(
            storage.path(),
            "worker-a",
            &identity,
            &generation,
            delta,
            Some(first),
        )
        .unwrap(),
    );
    // Post-fold trimming: the override key leaves the pin only because the
    // published generation's ready keys (entries and overrides) protect it.
    let published_keys = composed
        .ready_keys()
        .into_iter()
        .map(|key| key.to_hex())
        .collect();
    live.trim_published(&published_keys, |_| false).unwrap();
    assert!(live.is_empty(), "the published override key was trimmed");
    drop(composed);
    crate::views::wait_for_generation_maintenance_for_test(&worker_dir);
    for _ in 0..2 {
        sweep(storage.path());
        assert!(fill_present(), "the published override key was collected");
    }
    let reader = crate::views::base_refs::open_foreign_v3(
        storage.path(),
        FAMILY,
        "worker-a",
        &header().producers,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        reader.plane_readiness(
            &path("src/a.rs"),
            crate::blob_store::v2::FamilyPlane::Semantic
        ),
        Some(&crate::views::readiness::PlaneState::ready(&fill))
    );
    let _ = (key_of(b"alpha"), entry(b"alpha"), frontier());
}

#[test]
fn concurrent_binders_in_one_process_keep_every_published_reference() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (identity, generation) = publish_test_base(storage.path(), &[("src/a.rs", b"alpha")]);
    let payloads: Vec<Vec<u8>> = (0..4u8).map(|index| vec![b'w', index]).collect();
    put_rows(
        storage.path(),
        &payloads.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    );
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4u8)
            .map(|index| {
                let storage = storage.path();
                let identity = identity.clone();
                let generation = generation.clone();
                let bytes = payloads[index as usize].clone();
                scope.spawn(move || {
                    let mut forged = identity.clone();
                    // One binder aborts at verification.
                    if index == 3 {
                        forged.identity = [0; 32];
                    }
                    bind(
                        storage,
                        &format!("worker-{index}"),
                        &forged,
                        &generation,
                        worker_delta(&identity, &generation, &bytes),
                        None,
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    let mut kept = Vec::new();
    for (index, outcome) in outcomes.into_iter().enumerate() {
        match (index, outcome) {
            (3, Err(_)) => {}
            (_, Ok(WorkerOutcome::Published { composed, .. })) => kept.push(composed),
            (index, other) => panic!("binder {index}: {other:?}"),
        }
    }
    let rows = refs(storage.path());
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows
        .iter()
        .all(|row| row.2 == REF_PUBLISHED && row.0 != "worker-3"));
    drop(kept);
    for _ in 0..3 {
        let report = sweep(storage.path());
        assert!(
            report.reclaimed_bases.is_empty() && report.counted_bases.is_empty(),
            "{report:?}"
        );
        assert_eq!(report.valid_edges, 3);
    }
    assert!(plane_has(storage.path(), b"alpha"));
}

#[test]
fn an_injected_failure_at_each_sweep_boundary_never_reclaims_early_and_recovers() {
    use crate::views::contracts::{inject_fault_at, InjectedFault};
    for step in [
        DurabilityStep::BaseUnreferencedCounted,
        DurabilityStep::BaseReclaimBegun,
    ] {
        for fault in InjectedFault::ALL {
            let _serial = v3::serial_for_test();
            let storage = tempfile::tempdir().unwrap();
            let (identity, _) = publish_test_base(storage.path(), &[("a.rs", b"alpha")]);
            if step == DurabilityStep::BaseReclaimBegun {
                sweep(storage.path());
            }
            inject_fault_at(step, fault);
            assert!(
                sweep_family(storage.path(), FAMILY, None).is_err(),
                "{step:?} {fault:?}"
            );
            // The failed sweep released its lease; later sweeps finish the work
            // without ever reclaiming before the second unreferenced sweep.
            let mut reclaimed = false;
            for _ in 0..3 {
                let report = sweep(storage.path());
                if report.reclaimed_bases.contains(&identity.scope) {
                    reclaimed = true;
                    break;
                }
            }
            assert!(reclaimed, "{step:?} {fault:?}");
            assert!(!base_registered(storage.path(), &identity));
            assert!(!base_dir(storage.path(), &identity).exists());
        }
    }
}
