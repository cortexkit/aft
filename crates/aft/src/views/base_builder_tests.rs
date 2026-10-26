//! Build claim, live-base cap, binder plan, reservation sizing and v3
//! disk-budget admission.

use super::*;
use crate::views::base_refs::tests::{publish_test_base, FAMILY};
use crate::views::eviction::{DiskBudget, V3Refusal, V3_HARD_LIMIT_GAP};

const MIB: u64 = 1 << 20;

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

#[test]
fn the_initial_reservation_follows_the_ruled_formula_and_cap() {
    // The ruling's AFT fixture: 5,377 files / 87,031,923 bytes, no history.
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
        BinderPlan::Strict("live_base_cap")
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
fn v3_admission_refuses_past_the_hard_limit_with_the_named_gap_and_no_fallback() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    publish_test_base(storage.path(), &[("a.rs", b"alpha")]);
    let lock = v3::ReservationLock::acquire(storage.path()).unwrap();
    let mut registry = v3::RegistryWriter::open_existing(storage.path(), FAMILY, &lock).unwrap();
    let usage = registry.accounting().unwrap().usage();
    let budget = DiskBudget {
        family_store_hard: usage + 8 * MIB,
        family_payload: usage + 2 * MIB,
        ..DiskBudget::default()
    };
    let before = registry.accounting().unwrap();
    let refused = budget.admit_v3(&mut registry, 9 * MIB).unwrap_err();
    assert!(matches!(refused, V3Refusal::HardLimit { .. }));
    assert!(
        refused.to_string().starts_with(V3_HARD_LIMIT_GAP),
        "{refused}"
    );
    assert_eq!(
        registry.accounting().unwrap(),
        before,
        "a refusal reserves nothing"
    );
    let admitted = budget.admit_v3(&mut registry, 4 * MIB).unwrap();
    assert!(admitted.over_target);
    assert_eq!(
        registry.reserved_bytes(&admitted.reservation).unwrap(),
        4 * MIB
    );
    // Growth past the hard limit is refused before any byte is written.
    assert!(matches!(
        budget.extend_v3(&mut registry, &admitted, 5 * MIB),
        Err(V3Refusal::HardLimit { .. })
    ));
    budget.extend_v3(&mut registry, &admitted, 2 * MIB).unwrap();
    assert_eq!(
        registry.reserved_bytes(&admitted.reservation).unwrap(),
        6 * MIB
    );
    registry.release(admitted.reservation).unwrap();
    assert!(!storage.path().join("blobs/v2").exists());
}

#[cfg(any(unix, windows))]
#[test]
fn the_real_free_space_probe_reports_this_volume() {
    let storage = tempfile::tempdir().unwrap();
    let (free, capacity) =
        v3::probe_free_space_for_test(storage.path()).expect("supported platform");
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
    let tiny = DiskBudget {
        family_store_hard: MIB,
        ..DiskBudget::default()
    };
    let BaseBuild::Strict(reason) =
        build_base(input(storage.path(), root.path(), &paths, tiny), None).unwrap()
    else {
        panic!("over the hard limit");
    };
    assert!(reason.starts_with(V3_HARD_LIMIT_GAP), "{reason}");
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
