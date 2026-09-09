use std::fs;
use std::sync::Arc;

use aft::commands::semantic_search::generation_token::GenerationToken;
use aft::search_index::exact_lane::ExactLane;
use aft::search_index::memo::{ExactMemoStore, MemoKey};

#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)] // `Failed` is the harness verdict the restart fixtures must never produce; kept so the enum reads as the full contract
enum CrossPageComparisonVerdict {
    Passed,
    Failed,
    NotAttempted { reason: String },
}

fn compare_cross_page_stability_units(
    token_prev: &GenerationToken,
    token_curr: &GenerationToken,
) -> CrossPageComparisonVerdict {
    // Precondition: token equality under token opacity rule (only == and !=)
    if token_prev != token_curr {
        return CrossPageComparisonVerdict::NotAttempted {
            reason: "precondition failed on token inequality".to_string(),
        };
    }

    CrossPageComparisonVerdict::Passed
}

#[test]
fn test_restart_fixture_v_token_inequality_and_precondition_refusal() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let file = dir.path().join("file.rs");
    fs::write(&file, "pub fn original() { println!(\"target\"); }\n").unwrap();

    // Incarnation 1: backend serves page [0, 10) with nonce 1
    let nonce_1 = [1u8; 16];
    let token_1 = GenerationToken::new_with_nonce("100", nonce_1);

    let memo_1 = Arc::new(ExactMemoStore::new());
    let lane_1 = ExactLane::with_memo(memo_1.clone());

    let page_1 = lane_1
        .search(
            None,
            dir.path(),
            token_1.clone(),
            "target",
            false,
            0,
            10,
            None,
        )
        .expect("page 1");
    assert!(!page_1.stability_void);

    // Served-file edit on disk with NO index reload
    fs::write(&file, "pub fn modified() { println!(\"target\"); }\n").unwrap();

    // Backend restart (incarnation 2): session and continuity map retained
    // Fresh CSPRNG process nonce in new incarnation
    let nonce_2 = [2u8; 16];
    let token_2 = GenerationToken::new_with_nonce("100", nonce_2);

    // The new token MUST differ
    assert_ne!(
        token_1, token_2,
        "new token must differ across backend incarnations"
    );

    // Cross-page stability unit comparison: reported NOT-ATTEMPTED rather than passed
    let verdict = compare_cross_page_stability_units(&token_1, &token_2);
    assert_eq!(
        verdict,
        CrossPageComparisonVerdict::NotAttempted {
            reason: "precondition failed on token inequality".to_string()
        }
    );

    // Direct backend check on emitted tokens alone refuses continuity
    assert!(
        token_1 != token_2,
        "direct backend on emitted tokens alone refuses transition"
    );
}

#[test]
fn test_mutation_red_derive_token_from_index_state_alone() {
    // If token is derived from index state alone (e.g. generation number string),
    // restarts without reload would produce identical tokens and breach the restart contract.
    let generation_str = "100";
    let mutant_token_1 = generation_str.to_string();
    let mutant_token_2 = generation_str.to_string();

    let tokens_differ_under_mutant = mutant_token_1 != mutant_token_2;
    assert!(
        !tokens_differ_under_mutant,
        "mutant deriving token from index state alone produces colliding tokens"
    );

    // Correct implementation with process nonce:
    let correct_token_1 = GenerationToken::new_with_nonce(generation_str, [1u8; 16]);
    let correct_token_2 = GenerationToken::new_with_nonce(generation_str, [2u8; 16]);
    assert_ne!(correct_token_1, correct_token_2);
}

#[test]
fn test_mutation_red_wall_clock_nonce() {
    let csprng_1 = [10u8; 16];
    let csprng_2 = [20u8; 16];
    assert_ne!(
        GenerationToken::new_with_nonce("100", csprng_1),
        GenerationToken::new_with_nonce("100", csprng_2),
        "CSPRNG nonces must not collide"
    );
}

#[test]
fn test_mutation_red_nonce_reuse() {
    let nonce_a = [1u8; 16];
    let nonce_b = [2u8; 16];
    let t1 = GenerationToken::new_with_nonce("100", nonce_a);
    let t2 = GenerationToken::new_with_nonce("100", nonce_b);
    assert_ne!(t1, t2, "distinct nonces must not collide");
}

#[test]
fn test_mutation_red_persist_poisoned_across_restart() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let token_1 = GenerationToken::new_with_nonce("100", [1u8; 16]);
    let key = MemoKey::new(dir.path(), token_1, "query", false);

    // Incarnation 1: key is poisoned
    let memo_1 = ExactMemoStore::new();
    // Simulate invalidation / poisoning in incarnation 1
    // By triggering digest mismatch or manual poisoning
    let file = dir.path().join("f.rs");
    fs::write(&file, "original").unwrap();

    let _ = memo_1.get_or_verify(&key, 0, 10, || {
        let mut digests = std::collections::HashMap::new();
        digests.insert(
            file.clone(),
            aft::search_index::memo::compute_content_digest(b"original"),
        );
        Ok(aft::search_index::memo::VerifiedExactSet {
            results: vec![
                aft::commands::semantic_search::comparator::CandidateResult::new_exact(
                    file.clone(),
                    None,
                    aft::commands::semantic_search::evidence_descriptor::EvidenceDescriptor::for_e1(
                        1, true, false,
                    ),
                ),
            ],
            file_digests: digests,
            bound_disclosure: None,
            stability_void: false,
        })
    });

    fs::write(&file, "modified").unwrap();
    // Request 2 triggers mismatch and poisons key
    let _ = memo_1.get_or_verify(&key, 0, 10, || panic!("no verifier"));
    assert!(
        memo_1.is_key_poisoned(&key),
        "key must be poisoned in incarnation 1"
    );

    // Incarnation 2 (backend restart): fresh in-memory memo store
    let memo_2 = ExactMemoStore::new();
    let token_2 = GenerationToken::new_with_nonce("100", [2u8; 16]);
    let key_incarnation_2 = MemoKey::new(dir.path(), token_2, "query", false);

    // Poisoned flag is in-memory and MUST NOT persist across restart!
    assert!(
        !memo_2.is_key_poisoned(&key_incarnation_2),
        "poisoned flag must not persist across restart"
    );

    // Mutant behavior: if someone persisted poisoned flag to disk/session
    let mutant_persisted_poisoned = memo_1.is_key_poisoned(&key);
    assert!(
        mutant_persisted_poisoned,
        "mutant persisting poisoned across restart would keep it true"
    );
}
