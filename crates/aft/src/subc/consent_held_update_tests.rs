#[test]
fn held_update_v1_packaged_vectors_match_the_published_bytes() {
    use cortexkit_role_tool_provider::{
        late_results::{AckRequest, LateEntry, LateResultsReply},
        result::{self, ToolResult},
    };
    use sha2::Digest;
    let bytes = cortexkit_role_tool_provider::vectors::HELD_UPDATE_V1_JSON.as_bytes();
    assert_eq!(
        format!("{:x}", sha2::Sha256::digest(bytes)),
        "88d25951598e4fad4c65c737faac3f5f0254c13a871191c00ea9f4c9c396d230"
    );
    let vectors: Value = serde_json::from_slice(bytes).unwrap();
    let sequence = &vectors["sequence"];
    let first: ToolResult = serde_json::from_value(sequence["first_reply"].clone()).unwrap();
    result::validate(&first).unwrap();
    let reply: LateResultsReply = serde_json::from_value(sequence["reply"].clone()).unwrap();
    assert_eq!(serde_json::to_value(&reply).unwrap(), sequence["reply"]);
    let ack: AckRequest = serde_json::from_value(sequence["ack"].clone()).unwrap();
    assert_eq!(serde_json::to_value(ack).unwrap(), sequence["ack"]);
    let mut waits = Vec::new();
    let mut effective_at = Vec::new();
    let mut answer = None;
    for entry in &reply.entries {
        entry.validate().unwrap();
        if let Some(held) = entry.held().unwrap() {
            waits.push(result::is_person_wait(&held.reason));
            effective_at.push(entry.settled_at);
        } else {
            answer = Some(entry.event_id.clone());
        }
    }
    assert_eq!(json!(waits), sequence["person_wait"]);
    assert_eq!(json!(effective_at), sequence["effective_at"]);
    assert_eq!(json!(answer.unwrap()), sequence["answer_event_id"]);
    for case in vectors["valid"].as_array().unwrap() {
        let entry: LateEntry = serde_json::from_value(case["entry"].clone()).unwrap();
        entry.validate().unwrap();
        assert_eq!(
            serde_json::to_value(entry).unwrap(),
            case["entry"],
            "{}",
            case["name"]
        );
    }
    for case in vectors["invalid"].as_array().unwrap() {
        assert!(
            serde_json::from_value::<LateEntry>(case["entry"].clone())
                .and_then(|entry| entry.validate())
                .is_err(),
            "{}",
            case["name"]
        );
    }
    for case in vectors["gating"].as_array().unwrap() {
        let caps: Option<Vec<String>> =
            serde_json::from_value(case["consumer_capabilities"].clone()).unwrap();
        let may_send = case["first_reply_held"] == true
            && cortexkit_role_tool_provider::consumer_reads_held_update_v1(caps.as_deref());
        assert_eq!(json!(may_send), case["may_send"], "{}", case["name"]);
    }
}

#[test]
fn held_update_migration_preserves_schema_24_calls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let db = crate::db::open(&path).unwrap();
    let key = ledger::Key {
        carrier: "migration-test".into(),
        call_key: "kept".into(),
    };
    ledger::admit(
        &db,
        &key,
        "kept-digest",
        None,
        ledger::State::DispatchStarted,
    )
    .unwrap();
    db.execute_batch("DROP TABLE call_ledger_updates; UPDATE schema_version SET version=24;")
        .unwrap();
    drop(db);
    let db = crate::db::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT version FROM schema_version", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        25
    );
    let count: u64 = db
        .query_row("SELECT count(*) FROM call_ledger_updates", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        ledger::get(&db, &key).unwrap().unwrap().digest,
        "kept-digest"
    );
}

#[test]
fn held_updates_require_an_initial_held_reply() {
    let dir = tempfile::tempdir().unwrap();
    let db = crate::db::open(&dir.path().join("aft.db")).unwrap();
    let key = ledger::Key {
        carrier: "example:runner".into(),
        call_key: "not-held".into(),
    };
    let scope = ledger::ScopeIdentity {
        owner: "example:owner".into(),
        scope_ref: "scope".into(),
        scope_epoch: 1,
    };
    ledger::admit(&db, &key, "digest", Some(&scope), ledger::State::Prepared).unwrap();
    assert!(ledger::record_running_update(&db, &key, "approval", ledger::now_ms()).is_err());
    assert_eq!(
        db.query_row("SELECT count(*) FROM call_ledger_updates", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn approval_held_update_is_gated_durable_and_followed_by_one_terminal() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    for opted_in in [false, true] {
        let f = fixture(FakeCingulate::up());
        let controlled = Controlled::install(f.fake.clone());
        let mut run = program(
            &f,
            false,
            if opted_in {
                "run:updates"
            } else {
                "run:old-consumer"
            },
        );
        let mut capabilities = vec!["tool-result/v1".into(), "held-reply/v1".into()];
        if opted_in {
            capabilities.push("held-update/v1".into());
        }
        Arc::get_mut(&mut run.0).unwrap().held_update =
            cortexkit_role_tool_provider::consumer_reads_held_update_v1(Some(&capabilities));
        let mut replies = f
            .send_read(&run, &f.dir.path().join("elsewhere.txt"), 9200)
            .await;
        held(&mut replies).await;
        let request = controlled.request(0).await;
        let approved_at = ledger::now_ms();
        controlled.answer(&request, "allow_once");
        assert_ne!(late(&f, &run, 9200).await["isError"], true);
        let key = tool_provider::LedgerEdge::key(&run, "program-9200");
        let db = f.ctx.db().unwrap();
        let row = ledger::get(&db.lock().unwrap(), &key).unwrap().unwrap();
        let owner = row.scope.unwrap().owner;
        let feed = |conn: &rusqlite::Connection, incarnation: &str| -> Value {
            serde_json::from_slice(
                &ledger::late_results(
                    conn,
                    &owner,
                    incarnation,
                    None,
                    None,
                    ledger::now_ms(),
                    1024 * 1024,
                )
                .unwrap()
                .unwrap(),
            )
            .unwrap()
        };
        let original = feed(&db.lock().unwrap(), ledger::provider_incarnation());
        let entries = original["entries"].as_array().unwrap();
        assert_eq!(entries.len(), if opted_in { 2 } else { 1 });
        assert_eq!(entries.last().unwrap()["kind"], "result");
        if !opted_in {
            assert!(entries.iter().all(|entry| entry["kind"] != "held"));
            continue;
        }
        let update = &entries[0];
        assert_eq!(update["kind"], "held");
        assert_eq!(update["held"], json!({"reason":"running"}));
        assert_eq!(update["call_key"], "program-9200");
        assert_eq!(update["owner"], owner);
        assert_eq!(update["custodian"], owner);
        assert_eq!(update["ref"], run.scope.as_ref().unwrap().scope_ref);
        assert_eq!(
            update["scope_epoch"],
            run.scope.as_ref().unwrap().scope_epoch
        );
        let legacy_feed: Value = serde_json::from_slice(
            &ledger::late_results_for_consumer(
                &db.lock().unwrap(),
                &owner,
                ledger::provider_incarnation(),
                None,
                None,
                ledger::now_ms(),
                1024 * 1024,
                false,
            )
            .unwrap()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(legacy_feed["entries"].as_array().unwrap().len(), 1);
        assert_eq!(legacy_feed["entries"][0]["kind"], "result");
        assert!(update["settled_at"].as_i64().unwrap() >= approved_at);
        let typed: cortexkit_role_tool_provider::late_results::LateEntry =
            serde_json::from_value(update.clone()).unwrap();
        typed.validate().unwrap();
        let reopened = crate::db::open(&f.dir.path().join("store/aft.db")).unwrap();
        ledger::recover(&reopened).unwrap();
        assert!(!ledger::record_running_update(
            &reopened,
            &key,
            request["call_key"].as_str().unwrap(),
            ledger::now_ms()
        )
        .unwrap());
        let replay = feed(&reopened, "after-restart");
        assert_eq!(replay["entries"], original["entries"]);
        let old_cursor: cortexkit_role_tool_provider::late_results::Cursor =
            serde_json::from_value(original["cursor"].clone()).unwrap();
        assert_eq!(
            ledger::ack_through(&reopened, &owner, "after-restart", &old_cursor)
                .unwrap()
                .unwrap_err()
                .code,
            "cursor_incarnation_changed"
        );
        let update_seq: u64 = reopened
            .query_row(
                "SELECT seq FROM call_ledger_updates WHERE carrier=?1 AND call_key=?2",
                rusqlite::params![key.carrier, key.call_key],
                |row| row.get(0),
            )
            .unwrap();
        let cursor = cortexkit_role_tool_provider::late_results::Cursor {
            provider_incarnation: "after-restart".into(),
            seq: update_seq,
        };
        ledger::ack_through(&reopened, &owner, "after-restart", &cursor)
            .unwrap()
            .unwrap();
        assert!(!ledger::record_running_update(
            &reopened,
            &key,
            request["call_key"].as_str().unwrap(),
            ledger::now_ms()
        )
        .unwrap());
        let acked = feed(&reopened, "after-restart");
        assert_eq!(acked["entries"].as_array().unwrap().len(), 1);
        assert_eq!(acked["entries"][0]["kind"], "result");
    }
}
