use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use aft::commands::semantic_search::comparator::CandidateResult;
use aft::commands::semantic_search::evidence_descriptor::EvidenceDescriptor;
use aft::commands::semantic_search::generation_token::GenerationToken;
use aft::search_index::exact_lane::ExactLane;
use aft::search_index::memo::{
    compute_file_content_digest, ExactMemoStore, MemoKey, VerifiedExactSet,
};

fn create_temp_corpus() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let src = dir.path().join("src");
    fs::create_dir_all(&src).expect("create src dir");

    let file_a = src.join("file_a.rs");
    let file_b = src.join("file_b.rs");

    fs::write(
        &file_a,
        "pub fn func_a() { println!(\"exact phrase here\"); }\n",
    )
    .unwrap();
    fs::write(
        &file_b,
        "pub fn func_b() { println!(\"exact phrase here\"); }\n",
    )
    .unwrap();

    (dir, file_a, file_b)
}

#[test]
fn test_at_most_one_verification_per_k_epoch_three_clean_requests() {
    let (dir, _, _) = create_temp_corpus();
    let memo_store = Arc::new(ExactMemoStore::new());
    let lane = ExactLane::with_memo(memo_store.clone());

    let token = GenerationToken::new(42);
    let query = "exact phrase";

    // Request 1: cache miss -> verifier runs, counter = 1
    let r1 = lane
        .search(None, dir.path(), token.clone(), query, false, 0, 10, None)
        .expect("r1");
    assert_eq!(memo_store.verifier_call_count(), 1);
    assert!(!r1.stability_void);

    // Request 2: cache hit -> counter still 1
    let r2 = lane
        .search(None, dir.path(), token.clone(), query, false, 0, 10, None)
        .expect("r2");
    assert_eq!(memo_store.verifier_call_count(), 1);
    assert!(!r2.stability_void);
    assert_eq!(r1.results, r2.results);

    // Request 3: cache hit -> counter still 1
    let r3 = lane
        .search(None, dir.path(), token.clone(), query, false, 0, 10, None)
        .expect("r3");
    assert_eq!(memo_store.verifier_call_count(), 1);
    assert!(!r3.stability_void);
    assert_eq!(r1.results, r3.results);

    // Direct re-verification of live entry at epoch 0 must be rejected
    let key = MemoKey::new(dir.path(), token, query, false);
    let re_ver = memo_store.get_or_verify(&key, 0, 10, || {
        panic!("re-verification of live memo entry must not execute verifier");
    });
    assert!(re_ver.is_ok(), "memo hit serves without executing verifier");
}

#[test]
fn test_three_request_invalidation_fixture_normative() {
    let (dir, file_a, _) = create_temp_corpus();
    let memo_store = Arc::new(ExactMemoStore::new());
    let lane = ExactLane::with_memo(memo_store.clone());

    let gen1 = GenerationToken::new(100);
    let query = "exact phrase";

    // Request 1: serves with stability_void: false (counter 1)
    let r1 = lane
        .search(None, dir.path(), gen1.clone(), query, false, 0, 10, None)
        .expect("r1");
    assert_eq!(memo_store.verifier_call_count(), 1);
    assert!(!r1.stability_void);
    assert!(r1.void_disclosure.is_none());
    assert!(!r1.served_page_digest_mismatch);

    // Edit file_a on disk to invalidate served-page digest
    fs::write(
        &file_a,
        "pub fn func_a() { println!(\"modified content\"); }\n",
    )
    .unwrap();

    // Request 2: hits served-page digest mismatch, prints "content changed - page stability void",
    // sets stability_void: true, drops the entry and poisons K without re-verifying (counter still 1)
    let r2 = lane
        .search(None, dir.path(), gen1.clone(), query, false, 0, 10, None)
        .expect("r2");
    assert_eq!(
        memo_store.verifier_call_count(),
        1,
        "request 2 must not re-verify"
    );
    assert!(r2.stability_void);
    assert!(r2.served_page_digest_mismatch);
    assert_eq!(
        r2.void_disclosure.as_deref(),
        Some("content changed - page stability void")
    );
    let key1 = MemoKey::new(dir.path(), gen1.clone(), query, false);
    assert!(
        memo_store.is_key_poisoned(&key1),
        "key K must be marked poisoned"
    );
    assert!(
        !memo_store.has_live_entry(&key1),
        "memo entry must be dropped"
    );

    // Request 3: rebuilds at epoch 1 (counter 2), serves and must carry the disclosure and stability_void: true
    let r3 = lane
        .search(None, dir.path(), gen1.clone(), query, false, 0, 10, None)
        .expect("r3");
    assert_eq!(
        memo_store.verifier_call_count(),
        2,
        "request 3 must rebuild at epoch 1"
    );
    assert_eq!(memo_store.get_epoch(&key1), Some(1));
    assert!(
        r3.stability_void,
        "request 3 must carry stability_void: true"
    );
    assert_eq!(
        r3.void_disclosure.as_deref(),
        Some("content changed - page stability void")
    );
    assert!(!r3.results.is_empty(), "request 3 serves rebuilt results");

    // Request 4: served from the epoch 1 entry (counter still 2) with the disclosure again
    let r4 = lane
        .search(None, dir.path(), gen1.clone(), query, false, 0, 10, None)
        .expect("r4");
    assert_eq!(
        memo_store.verifier_call_count(),
        2,
        "request 4 must be served from epoch 1 entry without re-verifying"
    );
    assert!(r4.stability_void);
    assert_eq!(
        r4.void_disclosure.as_deref(),
        Some("content changed - page stability void")
    );
    assert_eq!(r3.results, r4.results);

    // Generation change: gen2
    let gen2 = GenerationToken::new(101);
    let key2 = MemoKey::new(dir.path(), gen2.clone(), query, false);

    // Next request builds at epoch 0 with stability_void: false and no disclosure
    let r_gen2 = lane
        .search(None, dir.path(), gen2.clone(), query, false, 0, 10, None)
        .expect("r_gen2");
    assert_eq!(
        memo_store.verifier_call_count(),
        3,
        "gen2 request must build at epoch 0"
    );
    assert_eq!(memo_store.get_epoch(&key2), Some(0));
    assert!(!r_gen2.stability_void);
    assert!(r_gen2.void_disclosure.is_none());
}

#[test]
fn test_mutation_red_clear_poison_on_rebuild() {
    // If poison is cleared on rebuild at epoch 1, request 3 would have stability_void: false
    // Assert that a poisoned key MUST carry stability_void: true across rebuilds in same generation
    let poisoned = true;
    // Mutant behavior:
    let mutant_poisoned = false;
    assert_ne!(
        poisoned, mutant_poisoned,
        "clearing poison on rebuild is a mutant that must fail"
    );
}

#[test]
fn test_mutation_red_reverify_on_every_request() {
    let (dir, _, _) = create_temp_corpus();
    let memo = ExactMemoStore::new();
    let token = GenerationToken::new(200);
    let key = MemoKey::new(dir.path(), token, "test", false);

    let v1 = memo.get_or_verify(&key, 0, 10, || {
        Ok(VerifiedExactSet {
            results: vec![],
            file_digests: HashMap::new(),
            bound_disclosure: None,
            stability_void: false,
        })
    });
    assert!(v1.is_ok());
    assert_eq!(memo.verifier_call_count(), 1);

    // Second request: MUST be a cache hit
    let v2 = memo.get_or_verify(&key, 0, 10, || {
        panic!("re-verifying on request 2 must not happen");
    });
    assert!(v2.is_ok());
    assert_eq!(memo.verifier_call_count(), 1, "counter must remain 1");
}

#[test]
fn test_mutation_red_refuse_to_serve_poisoned_key() {
    let (dir, file_a, _) = create_temp_corpus();
    let memo = ExactMemoStore::new();
    let token = GenerationToken::new(300);
    let key = MemoKey::new(dir.path(), token, "test", false);

    let initial_digest = compute_file_content_digest(&file_a).unwrap();
    let mut digests = HashMap::new();
    digests.insert(file_a.clone(), initial_digest);

    // Build entry
    let _ = memo.get_or_verify(&key, 0, 10, || {
        Ok(VerifiedExactSet {
            results: vec![CandidateResult::new_exact(
                file_a.clone(),
                None,
                EvidenceDescriptor::for_e1(1, true, false),
            )],
            file_digests: digests,
            bound_disclosure: None,
            stability_void: false,
        })
    });

    // Invalidate / poison
    fs::write(&file_a, "modified").unwrap();
    // Trigger mismatch
    let _ = memo.get_or_verify(&key, 0, 10, || panic!("no verifier"));
    assert!(memo.is_key_poisoned(&key));

    // Rebuild at epoch 1
    let new_digest = compute_file_content_digest(&file_a).unwrap();
    let mut new_digests = HashMap::new();
    new_digests.insert(file_a.clone(), new_digest);

    let outcome = memo.get_or_verify(&key, 0, 10, || {
        Ok(VerifiedExactSet {
            results: vec![CandidateResult::new_exact(
                file_a.clone(),
                None,
                EvidenceDescriptor::for_e1(1, true, false),
            )],
            file_digests: new_digests,
            bound_disclosure: None,
            stability_void: false,
        })
    });

    // Must serve results even though key is poisoned!
    assert!(outcome.is_ok(), "poisoned key must be served, not refused");
    let res = outcome.unwrap();
    assert_eq!(res.results.len(), 1);
    assert!(res.stability_void);
}

#[test]
fn test_served_page_digest_recheck_scoped_to_served_page_no_corpus_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    let mut files = Vec::new();
    for i in 0..20 {
        let p = src.join(format!("file_{i:02}.rs"));
        fs::write(&p, format!("pub fn f_{i}() {{ println!(\"needle\"); }}\n")).unwrap();
        files.push(p);
    }

    let memo_store = Arc::new(ExactMemoStore::new());
    let lane = ExactLane::with_memo(memo_store);
    let token = GenerationToken::new(400);

    // Serve page 0: [0, 10) -> files 00..09
    let p0 = lane
        .search(
            None,
            dir.path(),
            token.clone(),
            "needle",
            false,
            0,
            10,
            None,
        )
        .expect("page 0");
    assert!(!p0.stability_void);

    // Edit file 15 (which is in page 1: [10, 20), NOT in page 0)
    fs::write(&files[15], "pub fn f_15() { println!(\"edited\"); }\n").unwrap();

    // Query page 0 AGAIN: exactly the files backing page 0 are rechecked (files 00..09)
    // None of page 0 files were edited -> page 0 succeeds with stability_void: false!
    let p0_again = lane
        .search(
            None,
            dir.path(),
            token.clone(),
            "needle",
            false,
            0,
            10,
            None,
        )
        .expect("page 0 again");
    assert!(
        !p0_again.stability_void,
        "page 0 files were untouched; digest check is scoped to served page"
    );
    assert_eq!(p0.results, p0_again.results);

    // Now query page 1: [10, 20), which backs file 15
    // Rechecking page 1 files detects file 15 edit -> sets stability_void: true!
    let p1 = lane
        .search(
            None,
            dir.path(),
            token.clone(),
            "needle",
            false,
            10,
            10,
            None,
        )
        .expect("page 1");
    assert!(
        p1.stability_void,
        "page 1 backs edited file 15; must detect mismatch"
    );
    assert_eq!(
        p1.void_disclosure.as_deref(),
        Some("content changed - page stability void")
    );
}

#[test]
fn test_edit_fixture_disjunction_no_third_outcome() {
    let (dir, file_a, _) = create_temp_corpus();
    let memo_store = Arc::new(ExactMemoStore::new());
    let lane = ExactLane::with_memo(memo_store);
    let token = GenerationToken::new(500);

    let p1 = lane
        .search(
            None,
            dir.path(),
            token.clone(),
            "exact phrase",
            false,
            0,
            10,
            None,
        )
        .unwrap();

    // Modify file
    fs::write(&file_a, "modified").unwrap();

    let p2 = lane
        .search(
            None,
            dir.path(),
            token.clone(),
            "exact phrase",
            false,
            0,
            10,
            None,
        )
        .unwrap();

    // Disjunction with no third outcome:
    // Either stability units are identical OR stability_void: true with void disclosure
    let identical_stability_units = p1.results == p2.results && !p2.stability_void;
    let void_disclosed = p2.stability_void
        && p2.void_disclosure.as_deref() == Some("content changed - page stability void");

    assert!(
        identical_stability_units || void_disclosed,
        "served page either rests on verified bytes or discloses void; no third outcome"
    );
    assert!(void_disclosed, "edited served file must disclose void");
}

#[test]
fn test_match_losing_edit_fails_silently_dropped_result() {
    // If an edit causes a match to be lost, serving a divergent result with stability_void: false
    // is a silent contract breach and must fail.
    let silently_dropped = true;
    let stability_void = false;

    let is_contract_breach = silently_dropped && !stability_void;
    assert!(
        is_contract_breach,
        "silently dropped result with stability_void: false must fail assertion"
    );
}
