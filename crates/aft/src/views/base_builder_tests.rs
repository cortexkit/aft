//! Build claim, live-base cap, binder plan, reservation sizing and v3
//! disk-budget admission.

use super::*;
use crate::views::base_refs::tests::{publish_test_base, FAMILY};
use crate::views::eviction::{DiskBudget, V3Refusal};

const MIB: u64 = 1 << 20;

#[test]
fn directory_base_source_error_names_operation_and_path() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.rs");
    std::fs::create_dir(&path).unwrap();
    let error = read_stable(&path).unwrap_err().to_string();
    assert!(error.contains("reading base source"), "{error}");
    assert!(error.contains(&path.display().to_string()), "{error}");
    assert!(path.is_dir());
}

fn scope(byte: u8) -> String {
    crate::views::base_refs::base_scope_for(&[byte; 32])
}

fn dead_claim(storage: &Path, scope: &str, token: &str, attempt: i64) {
    with_base_barrier(storage, FAMILY, |tx| {
        tx.execute(
            "INSERT INTO base_builds VALUES(?1,?2,1,?3,?4,'building',0)",
            params![scope, i32::MAX as u32 - 9, token, attempt],
        )?;
        Ok(())
    })
    .unwrap();
}

fn capped_bases(storage: &Path) -> Vec<String> {
    crate::root_cache::register_durable_owner(&crate::root_cache::StoragePaths::same(storage))
        .unwrap();
    let scopes: Vec<_> = (0..LIVE_BASES_PER_FAMILY)
        .map(|index| crate::views::base_refs::base_scope_for(&[index as u8; 32]))
        .collect();
    with_base_barrier(storage, FAMILY, |tx| {
        for scope in &scopes {
            let (dir, _) = register_base_in(tx, storage, FAMILY, scope)?;
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("base-payload"), b"unused base")?;
        }
        Ok(())
    })
    .unwrap();
    scopes
}

fn registered_bases(storage: &Path) -> Vec<String> {
    v3::LedgerReader::open_existing(storage, FAMILY)
        .unwrap()
        .read(|connection| {
            Ok(connection
                .prepare("SELECT scope FROM members WHERE kind='base' ORDER BY scope")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap()
}

#[test]
fn live_base_cap_reclaims_unreferenced_bases_then_admits() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let scopes = capped_bases(storage.path());
    let walks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let _hook = crate::storage_retention::observe_test_hook(
        storage.path(),
        std::sync::Arc::new({
            let walks = walks.clone();
            move |step| {
                if step == "v3-pressure-reclaim-started" {
                    walks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }),
    );
    assert!(matches!(
        claim_base(storage.path(), FAMILY, &scope(200), None).unwrap(),
        ClaimOutcome::Claimed(_)
    ));
    assert!(registered_bases(storage.path()).len() < LIVE_BASES_PER_FAMILY as usize);
    assert!(scopes
        .iter()
        .any(|scope| !v3::view_dir(storage.path(), FAMILY, scope)
            .unwrap()
            .exists()));
    assert_eq!(walks.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn live_base_cap_reclaim_preserves_pins_and_live_builds() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let scopes = capped_bases(storage.path());
    let _pin = crate::pins::LivePin::create_v3(storage.path(), FAMILY, &scopes[0]).unwrap();
    let ClaimOutcome::Claimed(_build) =
        claim_base(storage.path(), FAMILY, &scopes[1], None).unwrap()
    else {
        panic!("a registered base can be claimed at the cap");
    };
    assert!(matches!(
        claim_base(storage.path(), FAMILY, &scope(200), None).unwrap(),
        ClaimOutcome::Claimed(_)
    ));
    let remaining = registered_bases(storage.path());
    assert!(remaining.contains(&scopes[0]));
    assert!(remaining.contains(&scopes[1]));
    assert!(remaining.len() < scopes.len());
    assert!(v3::view_dir(storage.path(), FAMILY, &scopes[0])
        .unwrap()
        .exists());
    assert!(v3::view_dir(storage.path(), FAMILY, &scopes[1])
        .unwrap()
        .exists());
}

#[test]
fn live_base_cap_second_claim_within_cooldown_does_not_walk() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let scopes = capped_bases(storage.path());
    let mut pins: Vec<_> = scopes
        .iter()
        .map(|scope| crate::pins::LivePin::create_v3(storage.path(), FAMILY, scope).unwrap())
        .collect();
    let walks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let _hook = crate::storage_retention::observe_test_hook(
        storage.path(),
        std::sync::Arc::new({
            let walks = walks.clone();
            move |step| {
                if step == "v3-pressure-reclaim-started" {
                    walks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }),
    );
    assert!(matches!(
        claim_base(storage.path(), FAMILY, &scope(200), None).unwrap(),
        ClaimOutcome::CapReached { .. }
    ));
    assert_eq!(walks.load(std::sync::atomic::Ordering::SeqCst), 1);
    pins.pop();
    assert!(matches!(
        claim_base(storage.path(), FAMILY, &scope(201), None).unwrap(),
        ClaimOutcome::CapReached { .. }
    ));
    assert_eq!(walks.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(registered_bases(storage.path()), scopes);
    let _floor = crate::disk_floor::set_free_space_for_test(storage.path(), 0, 1 << 30);
    assert!(matches!(
        v3::publish(storage.path(), FAMILY, 1, None, |_| Ok(())),
        Err(v3::Error::Refused("device_free_space_floor"))
    ));
    assert_eq!(walks.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn the_initial_reservation_follows_the_ruled_formula_and_cap() {
    // The opening note's AFT fixture: 5,377 files / 87,031,923 bytes, no
    // history, which it says reserves 401 MiB.
    assert_eq!(initial_reservation(5_377, 87_031_923, None), 401 * MIB);
    // A 200,000-file repository is capped at 1 GiB, not refused.
    assert_eq!(
        initial_reservation(200_000, 4 << 30, None),
        INITIAL_RESERVATION_CAP
    );
    // History scales a comparable base's measured peak; zero inventory drops it.
    assert_eq!(
        initial_reservation(100, 1_000, Some((200 * MIB, 50, 1_000))),
        64 * MIB + 500 * MIB
    );
    assert_eq!(
        initial_reservation(100, 1_000, Some((400 * MIB, 0, 1_000))),
        initial_reservation(100, 1_000, None)
    );
}

#[test]
fn concurrent_different_bases_below_cap_never_wait_for_the_reclaim_gate() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let cooldown = v3::floor_reclaim_cooldown(storage.path(), FAMILY);
    // Keep the gate occupied so the regression is independent of thread timing.
    // Below the family's live-base count limit, neither claim needs reclamation.
    let _gate = cooldown.lock().unwrap();
    let start = std::sync::Barrier::new(3);
    std::thread::scope(|threads| {
        let first = threads.spawn(|| {
            start.wait();
            claim_base(storage.path(), FAMILY, &scope(201), None).unwrap()
        });
        let second = threads.spawn(|| {
            start.wait();
            claim_base(storage.path(), FAMILY, &scope(202), None).unwrap()
        });
        start.wait();
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert!(matches!(first, ClaimOutcome::Claimed(_)), "{first:?}");
        assert!(matches!(second, ClaimOutcome::Claimed(_)), "{second:?}");
    });
}

#[test]
fn capped_claim_retries_a_busy_reclaim_gate_at_the_live_claim_cadence() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    capped_bases(storage.path());
    let cooldown = v3::floor_reclaim_cooldown(storage.path(), FAMILY);
    let _gate = cooldown.lock().unwrap();
    assert!(matches!(
        claim_base(storage.path(), FAMILY, &scope(200), None).unwrap(),
        ClaimOutcome::Reclaiming
    ));
    for waited in [std::time::Duration::ZERO, LIVE_CLAIM_WAIT] {
        assert!(matches!(
            plan_binder(storage.path(), FAMILY, &scope(200), waited),
            BinderPlan::Wait { remaining } if remaining == CLAIM_HEARTBEAT
        ));
    }
}

#[test]
fn two_claimants_of_one_base_produce_exactly_one_build() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let target = scope(1);
    let builds = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|threads| {
        for _ in 0..4 {
            threads.spawn(
                || match claim_base(storage.path(), FAMILY, &target, None).unwrap() {
                    ClaimOutcome::Claimed(_claim) => {
                        builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    ClaimOutcome::Busy { .. } => {}
                    other => panic!("{other:?}"),
                },
            );
        }
    });
    assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn a_dead_builder_is_taken_over_once_with_attempt_plus_one_and_fresh_names() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let target = scope(2);
    dead_claim(storage.path(), &target, "deadtoken", 1);
    let staging = v3::view_dir(storage.path(), FAMILY, &target)
        .unwrap()
        .join("staging/a1-deadtoken-x");
    std::fs::create_dir_all(&staging).unwrap();
    let ClaimOutcome::Claimed(claim) = claim_base(storage.path(), FAMILY, &target, None).unwrap()
    else {
        panic!("a dead owner's claim must be taken over");
    };
    assert_eq!(claim.attempt(), 2);
    assert_ne!(claim.token(), "deadtoken");
    assert!(!staging.exists(), "the dead attempt's staging is reclaimed");
    let fresh = claim.staging_dir().unwrap();
    assert!(fresh
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with(&format!("a2-{}-", claim.token())));
    assert!(claim
        .derived_name("g")
        .ends_with(&format!("-a2-{}.sqlite", claim.token())));
    // The survivor is now the live owner: nobody else takes over.
    assert!(matches!(
        claim_base(storage.path(), FAMILY, &target, None).unwrap(),
        ClaimOutcome::Busy { attempt: 2, .. }
    ));
    assert!(claim.verify_held().unwrap());
    claim.heartbeat().unwrap();
    claim.release().unwrap();
    assert!(matches!(
        claim_base(storage.path(), FAMILY, &target, None).unwrap(),
        ClaimOutcome::Claimed(_)
    ));
}

#[test]
fn a_live_claim_holds_an_empty_registered_base_through_sweeps() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let target = scope(3);
    let ClaimOutcome::Claimed(claim) = claim_base(storage.path(), FAMILY, &target, None).unwrap()
    else {
        panic!()
    };
    with_base_barrier(storage.path(), FAMILY, |tx| {
        register_base_in(tx, storage.path(), FAMILY, &target).map(|_| ())
    })
    .unwrap();
    for _ in 0..3 {
        let report = crate::views::base_sweep::sweep_family(storage.path(), FAMILY, None).unwrap();
        assert!(
            report.counted_bases.is_empty() && report.reclaimed_bases.is_empty(),
            "{report:?}"
        );
    }
    claim.release().unwrap();
    crate::views::base_sweep::sweep_family(storage.path(), FAMILY, None).unwrap();
    let report = crate::views::base_sweep::sweep_family(storage.path(), FAMILY, None).unwrap();
    assert_eq!(report.reclaimed_bases, vec![target]);
}

#[test]
fn the_live_base_cap_counts_claims_and_registered_bases_and_records_hits() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    with_base_barrier(storage.path(), FAMILY, |tx| {
        for index in 0..LIVE_BASES_PER_FAMILY - 1 {
            register_base_in(
                tx,
                storage.path(),
                FAMILY,
                &format!("{}{index:02}", scope(4)),
            )?;
        }
        Ok(())
    })
    .unwrap();
    let _pins: Vec<_> = registered_bases(storage.path())
        .iter()
        .map(|scope| crate::pins::LivePin::create_v3(storage.path(), FAMILY, scope).unwrap())
        .collect();
    // One claimed-but-unregistered base fills the cap.
    assert!(matches!(
        claim_base(storage.path(), FAMILY, &scope(5), None).unwrap(),
        ClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        claim_base(storage.path(), FAMILY, &scope(6), None).unwrap(),
        ClaimOutcome::CapReached { live_bases } if live_bases == LIVE_BASES_PER_FAMILY
    ));
    let hits: i64 = v3::LedgerReader::open_existing(storage.path(), FAMILY)
        .unwrap()
        .read(|c| {
            Ok(
                c.query_row("SELECT live_base_cap_hits FROM sweep_state", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(hits, 1);
    assert!(matches!(
        plan_binder(storage.path(), FAMILY, &scope(6), std::time::Duration::ZERO),
        BinderPlan::Wait { remaining } if remaining == std::time::Duration::from_secs(60)
    ));
}

#[test]
fn binders_wait_for_a_live_claim_until_the_ruled_budget_then_go_strict() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let target = scope(7);
    let BinderPlan::Build(_owner) =
        plan_binder(storage.path(), FAMILY, &target, std::time::Duration::ZERO)
    else {
        panic!("the first binder builds");
    };
    assert!(matches!(
        plan_binder(storage.path(), FAMILY, &target, std::time::Duration::from_secs(1)),
        BinderPlan::Wait { remaining } if remaining == LIVE_CLAIM_WAIT - std::time::Duration::from_secs(1)
    ));
    assert!(matches!(
        plan_binder(storage.path(), FAMILY, &target, LIVE_CLAIM_WAIT),
        BinderPlan::Strict("live_claim_wait_exhausted")
    ));
}

#[test]
fn v3_admission_has_no_family_byte_limit_and_refuses_only_at_the_device_floor() {
    const GIB: u64 = 1024 * MIB;
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    publish_test_base(storage.path(), &[("a.rs", b"alpha")]);
    let lock = v3::ReservationLock::acquire(storage.path()).unwrap();
    let mut registry = v3::RegistryWriter::open_existing(storage.path(), FAMILY, &lock).unwrap();
    let usage = registry.accounting().unwrap().usage();
    let budget = DiskBudget {
        family_payload: usage + 2 * MIB,
        ..DiskBudget::default()
    };
    // A 6 GiB request with room on the device is admitted, since only the
    // device floor refuses; it exceeds the target, so the caller sweeps.
    let roomy = crate::disk_floor::set_free_space_for_test(storage.path(), 1024 * GIB, 2048 * GIB);
    let admitted = budget.admit_v3(&mut registry, 6 * GIB).unwrap();
    assert!(admitted.over_target);
    budget.extend_v3(&mut registry, &admitted, 3 * GIB).unwrap();
    assert_eq!(
        registry.reserved_bytes(&admitted.reservation).unwrap(),
        9 * GIB
    );
    registry.release(admitted.reservation).unwrap();
    drop(roomy);
    // A device short of its free-space floor refuses by name and reserves
    // nothing; growth into the floor is refused before any byte is written.
    let _tight = crate::disk_floor::set_free_space_for_test(
        storage.path(),
        crate::disk_floor::MIN_FREE_BYTES + 8 * MIB,
        2 * crate::disk_floor::MIN_FREE_BYTES,
    );
    let before = registry.accounting().unwrap();
    let refused = budget.admit_v3(&mut registry, 9 * MIB).unwrap_err();
    assert!(
        refused.to_string() == "storage_gap:device_free_space_floor",
        "{refused}"
    );
    assert_eq!(
        registry.accounting().unwrap(),
        before,
        "a refusal reserves nothing"
    );
    let admitted = budget.admit_v3(&mut registry, 4 * MIB).unwrap();
    assert!(matches!(
        budget.extend_v3(&mut registry, &admitted, 5 * MIB),
        Err(V3Refusal::Ledger(_))
    ));
    registry.release(admitted.reservation).unwrap();
    assert!(!storage.path().join("blobs/v2").exists());
}

#[cfg(any(unix, windows))]
#[test]
fn the_real_free_space_probe_reports_this_volume() {
    let storage = tempfile::tempdir().unwrap();
    let (free, capacity) =
        crate::disk_floor::probe_free_space_for_test(storage.path()).expect("supported platform");
    assert!(capacity > 0 && free <= capacity, "{free} / {capacity}");
}

fn policy() -> crate::blob_store::v2::TrigramPolicy {
    crate::blob_store::v2::TrigramPolicy {
        max_file_size: 1 << 20,
    }
}

fn checkout(files: &[(&str, &[u8])]) -> (tempfile::TempDir, Vec<crate::views::RelPath>) {
    let root = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for (name, bytes) in files {
        let path = root.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        paths.push(crate::views::RelPath::new(name.as_bytes().to_vec()).unwrap());
    }
    (root, paths)
}

fn input<'a>(
    storage: &'a Path,
    root: &'a Path,
    paths: &'a [crate::views::RelPath],
    budget: DiskBudget,
) -> BaseBuildInput<'a> {
    BaseBuildInput {
        storage,
        family: FAMILY,
        root,
        paths,
        policy: policy(),
        semantic_producer: Some("sem".into()),
        callgraph_producer: "graph".into(),
        ignore_fingerprint: None,
        history: None,
        budget,
    }
}

const FILES: &[(&str, &[u8])] = &[
    ("src/a.rs", b"fn alpha() {}"),
    ("src/b.rs", b"fn beta() {}"),
];

#[test]
fn a_base_is_built_from_strict_reads_and_first_published_with_its_lexical_plane() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (root, paths) = checkout(FILES);
    let BaseBuild::Published {
        identity,
        generation,
        claim,
    } = build_base(
        input(storage.path(), root.path(), &paths, DiskBudget::default()),
        None,
    )
    .unwrap()
    else {
        panic!("the only builder publishes");
    };
    assert_eq!(claim.attempt(), 1);
    let base =
        crate::views::base_refs::resolve_base(storage.path(), FAMILY, &identity.scope).unwrap();
    assert_eq!(base.generation(), generation);
    assert!(base.manifest().header().head_tree.is_none());
    let reader =
        v3::PlaneReader::open_existing(storage.path(), FAMILY, FamilyPlane::Trigram).unwrap();
    for (_, entry) in base.manifest().entries() {
        let Some(PlaneState::Ready { key }) = entry.plane_state(FamilyPlane::Trigram) else {
            panic!("lexical plane incomplete at first publication");
        };
        let key = crate::blob_store::v2::FamilyKey::new(
            FamilyPlane::Trigram,
            crate::blob_store::v2::parse_hex32(key).unwrap(),
        );
        assert!(reader.get(&key).unwrap().is_some());
        assert!(entry
            .plane_state(FamilyPlane::Callgraph)
            .unwrap()
            .is_pending());
    }
    // The admission reservation was released; only measured charges remain.
    let ledger = v3::LedgerReader::open_existing(storage.path(), FAMILY).unwrap();
    assert_eq!(ledger.accounting().unwrap().reserved_bytes, 0);
    // A second builder of the same identity, while the claim lives, does not build.
    assert!(matches!(
        build_base(
            input(storage.path(), root.path(), &paths, DiskBudget::default()),
            None
        )
        .unwrap(),
        BaseBuild::Busy
    ));
    assert!(!storage.path().join("views/v3").exists());
    assert!(!storage.path().join("blobs/v2").exists());
}

#[test]
fn concurrent_builders_of_one_base_build_it_exactly_once() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (root, paths) = checkout(FILES);
    let results: Vec<_> = std::thread::scope(|threads| {
        let handles: Vec<_> = (0..3)
            .map(|_| {
                threads.spawn(|| {
                    build_base(
                        input(storage.path(), root.path(), &paths, DiskBudget::default()),
                        None,
                    )
                    .unwrap()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    assert_eq!(
        results
            .iter()
            .filter(|built| matches!(built, BaseBuild::Published { .. }))
            .count(),
        1,
        "{results:?}"
    );
    assert!(results
        .iter()
        .all(|built| matches!(built, BaseBuild::Published { .. } | BaseBuild::Busy)));
}

#[test]
fn a_refused_admission_sends_the_bind_strict_with_the_named_gap_and_frees_the_claim() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (root, paths) = checkout(FILES);
    let tight = crate::disk_floor::set_free_space_for_test(
        storage.path(),
        crate::disk_floor::MIN_FREE_BYTES + 1,
        2 * crate::disk_floor::MIN_FREE_BYTES,
    );
    let BaseBuild::Strict(reason) = build_base(
        input(storage.path(), root.path(), &paths, DiskBudget::default()),
        None,
    )
    .unwrap() else {
        panic!("short of the device floor");
    };
    assert!(reason.contains("device_free_space_floor"), "{reason}");
    drop(tight);
    assert!(matches!(
        build_base(
            input(storage.path(), root.path(), &paths, DiskBudget::default()),
            None
        )
        .unwrap(),
        BaseBuild::Published { .. }
    ));
}

#[test]
fn a_dead_builder_after_binds_is_taken_over_once_and_its_successor_is_cold_equivalent() {
    use crate::views::base_refs::tests::{bind, published};
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let (root, paths) = checkout(FILES);
    let BaseBuild::Published {
        identity,
        generation,
        claim,
    } = build_base(
        input(storage.path(), root.path(), &paths, DiskBudget::default()),
        None,
    )
    .unwrap()
    else {
        panic!()
    };
    // Workers bind to the lexical-ready base.
    let base =
        crate::views::base_refs::resolve_base(storage.path(), FAMILY, &identity.scope).unwrap();
    let header = base.manifest().header().clone();
    drop(base);
    let delta = |scope_generation: &str| {
        crate::views::manifest_v2::ManifestV2::new_v3(
            header.clone(),
            Some(identity.header(scope_generation)),
            crate::views::manifest_v2::PublishedFrontier::default(),
        )
    };
    let mut workers = Vec::new();
    for scope in ["worker-1", "worker-2"] {
        let (_, composed) = published(
            bind(
                storage.path(),
                scope,
                &identity,
                &generation,
                delta(&generation),
                None,
            )
            .unwrap(),
        );
        drop(composed);
        workers.push(scope);
    }
    // The builder dies with graph and semantic planes pending: its claim row
    // now names a process that is gone.
    let dead_token = claim.token().to_owned();
    with_base_barrier(storage.path(), FAMILY, |tx| {
        tx.execute(
            "UPDATE base_builds SET owner_pid=?1,owner_start=1 WHERE owner_token=?2",
            params![i32::MAX as u32 - 11, dead_token],
        )?;
        Ok(())
    })
    .unwrap();
    drop(claim);
    // Lexical service continues before the takeover.
    let lexical = |scope: &str| {
        let reader = crate::views::base_refs::open_foreign_v3(
            storage.path(),
            FAMILY,
            scope,
            &header.producers,
        )
        .unwrap()
        .unwrap();
        reader
            .entries()
            .filter(|(_, entry)| {
                matches!(
                    entry.plane_state(FamilyPlane::Trigram),
                    Some(PlaneState::Ready { .. })
                )
            })
            .count()
    };
    assert_eq!(lexical("worker-1"), FILES.len());
    // With no new binds, the surviving workers race for the claim: exactly
    // one takes it over with attempt + 1.
    let outcomes: Vec<_> = std::thread::scope(|threads| {
        let handles: Vec<_> = workers
            .iter()
            .map(|_| {
                threads.spawn(|| claim_base(storage.path(), FAMILY, &identity.scope, None).unwrap())
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    let mut takeovers = outcomes.into_iter().filter_map(|outcome| match outcome {
        ClaimOutcome::Claimed(claim) => Some(claim),
        _ => None,
    });
    let survivor = takeovers.next().expect("one survivor takes over");
    assert!(takeovers.next().is_none(), "exactly one takeover");
    assert_eq!(survivor.attempt(), 2);
    assert_ne!(survivor.token(), dead_token);
    // The survivor completes the pending planes: graph ready, semantic hit a
    // model outage. Lexical stays served throughout.
    let graph_key = |path: &crate::views::RelPath| {
        crate::blob_store::v2::FamilyKey::new(
            FamilyPlane::Callgraph,
            *blake3::hash(path.as_bytes()).as_bytes(),
        )
    };
    let fill = |manifest: &mut crate::views::manifest_v2::ManifestV2| {
        for (path, entry) in manifest.entries_mut() {
            if let crate::views::manifest_v2::EntryV2::Regular { planes, .. } = entry {
                planes.callgraph = Some(PlaneState::ready(&graph_key(path)));
                if planes.semantic.is_some() {
                    planes.semantic = Some(PlaneState::Failed {
                        reason: "model backend unavailable".into(),
                        producer: "sem".into(),
                    });
                }
            }
        }
    };
    let successor = publish_successor(&survivor, fill, None).unwrap();
    assert_eq!(lexical("worker-2"), FILES.len());
    let base =
        crate::views::base_refs::resolve_base(storage.path(), FAMILY, &identity.scope).unwrap();
    assert_eq!(base.generation(), successor);
    // Cold equivalence: the successor equals a base built cold over the same
    // files with the same completions.
    let mut cold = crate::views::manifest_v2::ManifestV2::new_v3(
        header.clone(),
        None,
        crate::views::manifest_v2::PublishedFrontier::default(),
    );
    for (path, entry) in
        crate::views::base_refs::resolve_base(storage.path(), FAMILY, &identity.scope)
            .unwrap()
            .manifest()
            .entries()
    {
        let crate::views::manifest_v2::EntryV2::Regular { content, size, .. } = entry else {
            continue;
        };
        let bytes = std::fs::read(
            root.path()
                .join(std::str::from_utf8(path.as_bytes()).unwrap()),
        )
        .unwrap();
        assert_eq!(crate::blob_store::v2::ContentHash::of(&bytes), *content);
        let key = crate::blob_store::v2::TrigramKey {
            content: *content,
            policy: policy(),
        }
        .family_key();
        cold.insert(
            path.clone(),
            crate::views::manifest_v2::EntryV2::regular(
                *content,
                *size,
                crate::views::manifest_v2::EntryPlanes {
                    trigram: Some(PlaneState::ready(&key)),
                    semantic: entry.plane_state(FamilyPlane::Semantic).cloned(),
                    callgraph: Some(PlaneState::ready(&graph_key(path))),
                },
            ),
        )
        .unwrap();
    }
    assert_eq!(base.manifest(), &cold);
    // A forbidden transition (a Ready key changed) is refused.
    drop(base);
    let error = publish_successor(
        &survivor,
        |manifest| {
            for (_, entry) in manifest.entries_mut() {
                if let crate::views::manifest_v2::EntryV2::Regular { planes, .. } = entry {
                    planes.callgraph = Some(PlaneState::pending("regressed"));
                }
            }
        },
        None,
    )
    .unwrap_err();
    assert!(matches!(error, BaseRefError::Corrupt(_)), "{error}");
}
