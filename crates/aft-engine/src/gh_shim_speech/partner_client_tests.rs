use super::*;
use serde_json::json;

// These are replies from the supplied PLEX contract, not simulated mutation guards.
fn refusal(result: Value, code: &str, expected: &str) {
    let bytes = serde_json::to_vec(&json!({"status":"ok","data":{"result":result}})).unwrap();
    match parse_reply(&bytes) {
        Reply::Refused {
            code: actual,
            stage,
            message,
            ..
        } => {
            assert_eq!(actual, code);
            assert_eq!(stage, "task_authority");
            assert_eq!(message, expected);
        }
        _ => panic!("expected an approved typed PLEX refusal"),
    }
}

#[test]
fn plex_expected_binding_mismatch_maps_to_authority_admission_race() {
    refusal(json!({"status":"refused","refusal_code":"stale_authority{dimension: expected_binding}","code":"stale_authority","dimension":"expected_binding","transmission":"not_sent"}),
        "task_gh_authority_unavailable",
        "this command's bot-write permission cannot be verified (admission_raced); start the command again from a live session, and tell the operator if it repeats.");
}

#[test]
fn plex_expected_handle_mismatch_maps_to_identity_conflict() {
    refusal(json!({"status":"refused","refusal_code":"stale_authority{dimension: expected_handle}","code":"stale_authority","dimension":"expected_handle","transmission":"not_sent"}),
        "task_gh_identity_conflict",
        "this command's bot identity no longer matches the repository's bot (binding_changed); start the command again from a live session.");
}

#[test]
fn plex_invalid_expect_maps_to_unverifiable_authority() {
    refusal(json!({"status":"refused","code":"invalid_request","field":"expect.repo_binding_generation","transmission":"not_sent"}),
        "task_gh_authority_unavailable",
        "this command's bot-write permission cannot be verified (snapshot_invalid); start the command again from a live session, and tell the operator if it repeats.");
}

#[test]
fn plex_nonce_payload_mismatch_is_not_retried_with_a_new_expect() {
    refusal(json!({"status":"refused","refusal_code":"nonce_payload_mismatch","code":"nonce_payload_mismatch","transmission":"not_sent"}),
        "task_gh_authority_unavailable",
        "this command's bot-write permission cannot be verified (snapshot_invalid); start the command again from a live session, and tell the operator if it repeats.");
}

#[test]
fn plex_route_end_names_the_session() {
    refusal(json!({"status":"refused","refusal_code":"scope_ended","code":"scope_ended","scope_role":"route","transmission":"not_sent"}),
        "task_gh_scope_ended",
        "the session this command started from has ended (ended), so it can no longer write as the bot; start the command again from a live session.");
}

#[test]
fn plex_run_end_names_the_program() {
    refusal(json!({"status":"refused","refusal_code":"scope_ended","code":"scope_ended","scope_role":"run","transmission":"not_sent"}),
        "task_gh_scope_ended",
        "the codemode program this command started from has ended (ended), so it can no longer write as the bot; start the command again from a live session.");
}

#[test]
fn plex_delegation_withdrawal_maps_to_lasting_authority_refusal() {
    refusal(json!({"status":"refused","refusal_code":"delegation_withdrawn","code":"delegation_withdrawn","scope_role":"route","transmission":"not_sent"}),
        "task_gh_authority_unavailable",
        "this command's bot-write permission cannot be verified (delegation_withdrawn); start the command again from a live session, and tell the operator if it repeats.");
}

#[test]
fn plex_unsynced_run_maps_to_retryable_program_unknown() {
    refusal(json!({"status":"refused","refusal_code":"scope_not_synced","code":"scope_not_synced","scope_role":"run","transmission":"not_sent"}),
        "task_gh_scope_unknown",
        "AFT could not confirm that the codemode program this command started from is still active (not_synced); retry in a minute.");
}

#[test]
fn plex_unverifiable_route_maps_to_session_unknown() {
    refusal(json!({"status":"refused","refusal_code":"scope_unverifiable","code":"scope_unverifiable","scope_role":"route","transmission":"not_sent"}),
        "task_gh_scope_unknown",
        "AFT could not confirm that the session this command started from is still active (unverifiable); retry in a minute.");
}

#[test]
fn plex_non_child_run_maps_to_program_unverifiable() {
    refusal(json!({"status":"refused","refusal_code":"run_scope_not_child","code":"run_scope_not_child","scope_role":"run","transmission":"not_sent"}),
        "task_gh_scope_unknown",
        "AFT could not confirm that the codemode program this command started from is still active (unverifiable); retry in a minute.");
}

#[test]
fn plex_terminal_replay_is_completed_even_when_the_binding_generation_moved() {
    let bytes=serde_json::to_vec(&json!({"status":"ok","data":{"repo_binding_generation":12,"result":{"status":"completed","result":{"id":42,"body":"recorded result"}}}})).unwrap();
    match parse_reply(&bytes) {
        Reply::Completed { generation, result } => {
            assert_eq!(generation, Some(12));
            assert_eq!(result, json!({"id":42,"body":"recorded result"}));
        }
        _ => panic!("terminal replay must not become a new write or a binding refusal"),
    }
}

#[test]
fn plex_partial_run_refusal_is_posted_work_not_a_retryable_refusal() {
    let bytes = serde_json::to_vec(&json!({"status":"ok","data":{"result":{
        "status":"refused","refusal_code":"scope_ended","scope_role":"run",
        "legs_sent":["add_issue_comment"]
    }}}))
    .unwrap();
    match parse_reply(&bytes) {
        Reply::Completed { result, .. } => {
            assert_eq!(result["status"], "partial_posted");
            assert_eq!(result["posted_legs"], json!(["add_issue_comment"]));
            assert_eq!(result["scope_role"], "run");
            assert_eq!(result["refusal_code"], "scope_ended");
            match render_completed(&result) {
                Err(RouteOutcome::PartiallyApplied(output)) => {
                    assert_eq!(output,"Posted as the bot: [\"add_issue_comment\"].\nThe rest of this GitHub write did not finish (scope_ended). Check GitHub before trying again.\n");
                    assert!(!output.contains("refused"));
                    assert!(!output.contains("No GitHub write was sent"));
                }
                _ => panic!("posted legs need a partial-applied outcome, never success or refusal"),
            }
        }
        _ => panic!("a non-empty legs_sent is posted work, never a refusal or a retry"),
    }
}
