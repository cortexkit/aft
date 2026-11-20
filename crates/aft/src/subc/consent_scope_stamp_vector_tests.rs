use super::*;

#[test]
fn consent_vectors_folder_grant_withdraw() {
    // Owner withdraws settle as `access_withdrawn` on AFT's normal route;
    // withdrawing an already-settled ask returns its own settled record for
    // the race guard; a non-owner gets `elicitation_not_owner`.
    for (name, id) in [
        ("scope-owner-withdraw-request.json", "acc_5"),
        ("non-owner-withdraw-request.json", "acc_5"),
        ("settled-withdraw-request.json", "acc_4"),
    ] {
        assert_eq!(
            encode("consent.withdraw", &json!({"elicitation_id": id})).unwrap(),
            vector(name),
            "{name}"
        );
    }
    assert_eq!(
        decode_record(&decode_result(&vector("scope-owner-withdraw-reply.json")).unwrap()).unwrap(),
        Settlement::Refused("access_withdrawn".into())
    );
    assert_eq!(
        decode_record(&decode_result(&vector("settled-withdraw-reply.json")).unwrap()).unwrap(),
        Settlement::Allow {
            scope: false,
            by_user: true
        }
    );
    let refusal: Value =
        serde_json::from_slice(&vector("non-owner-withdraw-refusal.json")).unwrap();
    assert_eq!(refusal["code"], json!("elicitation_not_owner"));
    let notice = vector_json("notice-owner-withdrawn.json");
    assert_eq!(notice["purpose"], json!("withdrawn"));
    assert_eq!(notice["elicitation_id"], json!("acc_5"));
    assert_eq!(notice["asker"]["scope_ref"], json!("worker"));
    assert_eq!(notice["asker"]["scope_epoch"], json!(3));
    let stamps = vector_json("withdraw-bind-stamps.json");
    assert_eq!(
        stamps["scope-owner-withdraw"],
        json!("reserved:prefrontal-core")
    );
    assert_eq!(stamps["non-owner-withdraw"], json!("reserved:basal"));
    assert_eq!(
        stamps["settled-withdraw"],
        json!("reserved:prefrontal-core")
    );
}

#[test]
fn consent_vectors_bind_stamps_route_asks_under_the_worker_stamp() {
    let stamps = vector_json("bind-stamps.json");
    let mut worker: subc_protocol::scope::ScopeStamp =
        serde_json::from_value(stamps["worker"]["scope"].clone()).unwrap();
    let mut head: subc_protocol::scope::ScopeStamp =
        serde_json::from_value(stamps["head"]["scope"].clone()).unwrap();
    assert_eq!(stamps["worker"]["principal"], "reserved:aft");
    // The pinned samples predate the module rename; live routes use the current owner.
    let owner = subc_protocol::Principal::Reserved {
        module_id: "prefrontal".into(),
    };
    worker.owner = owner.clone();
    worker.parent.as_mut().unwrap().owner = owner.clone();
    head.owner = owner;
    let dir = tempfile::tempdir().unwrap();
    let worker_identity = identity(dir.path(), Some(worker.clone()));
    let head_identity = identity(dir.path(), Some(head));
    assert!(access_grants::alfonso_worker(&worker_identity));
    assert!(!access_grants::alfonso_worker(&head_identity));
    // Asks go out under the asking worker's own stamp.
    let sent = selector(&worker);
    assert_eq!(sent.scope_ref, "worker");
    assert_eq!(sent.scope_epoch, Some(3));
    assert_eq!(sent.owner, worker.owner);
}
