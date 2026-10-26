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
