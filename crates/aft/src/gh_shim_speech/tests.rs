use super::*;

#[test]
fn approved_identity_template_is_exact() {
    assert_eq!(IDENTITY_TEXT, "this command's bot identity no longer matches the repository's bot ({reason}); start the command again from a live session.");
}

#[test]
fn approved_policy_template_is_exact() {
    assert_eq!(POLICY_TEXT, "this GitHub write is not allowed by the routing policy the command started under, or by the current one ({reason}); use a permitted operation, or ask the operator to update the policy and start the command again.");
}

#[test]
fn approved_temporary_template_is_exact() {
    assert_eq!(TEMPORARY_TEXT, "bot writes from background commands are briefly unavailable ({reason}); retry in a few seconds.");
}

#[test]
fn approved_unavailable_template_is_exact() {
    assert_eq!(UNAVAILABLE_TEXT, "this command's bot-write permission cannot be verified ({reason}); start the command again from a live session, and tell the operator if it repeats.");
}

#[test]
fn approved_ended_template_is_exact() {
    assert_eq!(ENDED_TEXT, "the {scope_role} this command started from has ended ({reason}), so it can no longer write as the bot; start the command again from a live session.");
}

#[test]
fn approved_unknown_template_is_exact() {
    assert_eq!(UNKNOWN_TEXT, "AFT could not confirm that the {scope_role} this command started from is still active ({reason}); retry in a minute.");
}

#[test]
fn approved_kill_warning_is_exact() {
    assert_eq!(REVOCATION_NOT_DURABLE, "bot_write_revocation_not_durable: the block on this command's bot writes was not saved and lasts only until AFT restarts; check the command's state.");
}

#[test]
fn pending_worker_write_wording_is_pinned_in_one_place() {
    assert_eq!(
        WORKER_WRITE_REFUSED,
        "workers cannot write to GitHub; report what should be posted to your parent instead"
    );
    assert_eq!(WORKER_WRITE_CODE, "gh_shim_worker_write_refused");
}

#[test]
fn scope_roles_keep_wire_identity_separate_from_plain_language() {
    let refusal = Refusal::scoped(Code::ScopeEnded, "ended_previously", ScopeRole::Run);
    assert_eq!(refusal.wire_data()["scope_role"], "run");
    assert_eq!(refusal.wire_data()["reason"], "ended_previously");
    assert_eq!(refusal.text(), "the codemode program this command started from has ended (ended_previously), so it can no longer write as the bot; start the command again from a live session.");
    assert_eq!(Refusal::scoped(Code::ScopeUnknown, "not_synced", ScopeRole::Head).text(), "AFT could not confirm that the head session this command started from is still active (not_synced); retry in a minute.");
    assert_eq!(ScopeRole::Authority.label(), "session");
}

#[test]
fn temporary_reasons_and_lasting_reasons_use_different_approved_templates() {
    for reason in [
        "serving_lock_unavailable",
        "storage_busy",
        "revocation_pending",
        "dispatch_queue_full",
        "reservation_timeout",
        "route_closed",
        "relay_unavailable",
    ] {
        assert_eq!(
            Refusal::new(Code::AuthorityUnavailable, reason).text(),
            format!("bot writes from background commands are briefly unavailable ({reason}); retry in a few seconds."),
        );
    }
    assert_eq!(Refusal::new(Code::AuthorityUnavailable, "snapshot_invalid").text(), "this command's bot-write permission cannot be verified (snapshot_invalid); start the command again from a live session, and tell the operator if it repeats.");
}

#[test]
fn typed_refusal_has_five_closed_code_families_and_the_task_stage() {
    for (code, expected) in [
        (Code::IdentityConflict, "task_gh_identity_conflict"),
        (Code::PolicyDenied, "task_gh_policy_denied"),
        (Code::AuthorityUnavailable, "task_gh_authority_unavailable"),
        (Code::ScopeEnded, "task_gh_scope_ended"),
        (Code::ScopeUnknown, "task_gh_scope_unknown"),
    ] {
        assert_eq!(code.as_str(), expected);
        assert_eq!(Code::parse(expected), Some(code));
    }
    assert_eq!(Code::parse("task_gh_allow_unknown"), None);
    assert_eq!(
        Refusal::new(Code::IdentityConflict, "binding_changed").wire_data()["stage"],
        "task_authority"
    );
}
