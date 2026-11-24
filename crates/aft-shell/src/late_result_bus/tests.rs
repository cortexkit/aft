use super::*;
use crate::db::call_ledger as ledger;
use cortexkit_bus_naming::AccountNames;
use serde_json::{json, Value};
use subc_protocol::FrameType;

#[derive(Default)]
struct RecordingPublisher {
    attempts: Vec<(String, String, Vec<u8>)>,
    drop_next: bool,
    fail: bool,
    stall: bool,
}

impl Publisher for RecordingPublisher {
    async fn publish(
        &mut self,
        reader: &str,
        msg_id: &str,
        body: Vec<u8>,
    ) -> Result<(), &'static str> {
        self.attempts.push((reader.into(), msg_id.into(), body));
        if self.stall {
            return std::future::pending().await;
        }
        if self.fail || std::mem::take(&mut self.drop_next) {
            Err("bus_publish")
        } else {
            Ok(())
        }
    }
}

fn key(call_key: &str) -> ledger::Key {
    ledger::Key {
        carrier: "reserved:broca".into(),
        call_key: call_key.into(),
    }
}

fn frame() -> ledger::RecordedFrame {
    ledger::RecordedFrame { ty: FrameType::Response,
        body: serde_json::to_vec(&json!({"content":[{"type":"text","text":"private tool output /private/project/file"}],"isError":false})).unwrap() }
}

fn admit(conn: &Connection, key: &ledger::Key, custodian: &str) {
    // Populate the call-ledger fields needed to admit a scoped call; the schema's
    // constraints still apply, and the production settlement path writes the feed.
    conn.execute("INSERT INTO call_ledger(carrier,call_key,digest,state,owner,scope_ref,scope_epoch,custodian,event_id) VALUES (?1,?2,'digest','Prepared',?3,'private/scope',1,?3,?4)",
        params![key.carrier, key.call_key, custodian, format!("settlement-{}", key.call_key)]).unwrap();
}

fn database() -> (tempfile::TempDir, crate::db::lifecycle::TrackedConnection) {
    let dir = tempfile::tempdir().unwrap();
    let db = crate::db::open(&dir.path().join("aft.db")).unwrap();
    (dir, db)
}

impl LedgerDatabase for aft_db::db::TrackedConnection {
    fn with_aft_db(
        &self,
        read: &mut dyn FnMut(&Self) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        read(self)
    }
}

#[tokio::test]
async fn prefrontal_rename_bus_preserves_custody_and_reader_subjects() {
    let (_dir, conn) = database();
    let names = AccountNames::derive("box_test").unwrap();
    for module in ["prefrontal-core", "prefrontal"] {
        let key = key(module);
        let principal = format!("reserved:{module}");
        admit(&conn, &key, &principal);
        ledger::settle(&conn, &key, &frame(), "ok", None, ledger::now_ms()).unwrap();
        let before = ledger::get(&conn, &key).unwrap().unwrap();
        let mut publisher = RecordingPublisher::default();
        drain(&conn, &mut DeliveryState::default(), &mut publisher).await;
        let (reader, _, _) = publisher
            .attempts
            .iter()
            .find(|(reader, _, _)| reader == module)
            .unwrap();
        assert_eq!(
            names
                .event_subject_with_reader(
                    "aft",
                    LATE_RESULT_READY_EVENT,
                    LATE_RESULT_READY_VERSION.try_into().unwrap(),
                    reader,
                )
                .unwrap(),
            format!("ck.box_test.event.aft.late_result_ready.v1.{module}")
        );
        let after = ledger::get(&conn, &key).unwrap().unwrap();
        assert_eq!(after.scope, before.scope);
        assert_eq!(after.custodian, before.custodian);
        assert_eq!(after.custodian.as_deref(), Some(principal.as_str()));
    }
}

#[test]
fn prefrontal_rename_aft_subscriptions_do_not_filter_on_head_identity() {
    let filters: Vec<_> = cortexkit_bus_naming::EVENT_CONSUMERS
        .iter()
        .filter(|row| row.module_id == "aft")
        .flat_map(|row| row.filter_subjects.iter().copied())
        .collect();
    assert_eq!(
        filters,
        [
            "event.thalamus-harness.send_state.v1",
            "event.broca.send_state.v1.aft",
            "event.cingulate.card_settled.v1.aft",
        ]
    );
}

async fn drain(
    conn: &dyn LedgerDatabase,
    state: &mut DeliveryState,
    publisher: &mut RecordingPublisher,
) -> usize {
    let mut visited = 0;
    loop {
        let capacity = state.capacity();
        if capacity == 0 {
            break;
        }
        let batch = scan(conn, &state.position, capacity).unwrap();
        visited += batch.visited;
        if !state.deliver(publisher, batch).await {
            break;
        }
    }
    visited
}

async fn retry_due(
    conn: &dyn LedgerDatabase,
    state: &mut DeliveryState,
    publisher: &mut RecordingPublisher,
) {
    let retained = read_retained(conn, state.due()).unwrap();
    state.retry(publisher, retained).await;
}

#[derive(Serialize)]
struct LegacyBody {
    call_key: String,
    outcome: String,
}

#[test]
fn late_result_bus_legacy_and_contract_serialization_match() {
    for (call_key, outcome) in [
        ("call:ok", "ok"),
        ("call:error", "tool_denied"),
        ("call:unknown", "unknown"),
    ] {
        let old = LegacyBody {
            call_key: call_key.into(),
            outcome: outcome.into(),
        };
        let new = cortexkit_role_tool_provider::LateResultReady::new(call_key, outcome);
        assert_eq!(
            serde_json::to_vec(&old).unwrap(),
            serde_json::to_vec(&new).unwrap()
        );
    }
}

#[tokio::test]
async fn late_result_bus_publisher_matches_provider_vectors() {
    use cortexkit_role_tool_provider::vectors::late_result_ready::FILES;

    fn fixture(name: &str) -> &'static str {
        FILES
            .iter()
            .find_map(|(path, contents)| (*path == name).then_some(*contents))
            .unwrap_or_else(|| panic!("missing provider vector: {name}"))
    }

    for (case, call_key, event_id, outcome) in [
        ("ok", "call:ok", "settlement:1", Some("ok")),
        ("error", "call:error", "settlement:2", Some("tool_denied")),
        ("unknown", "call:unknown", "settlement:3", None),
    ] {
        let (_dir, conn) = database();
        let key = key(call_key);
        admit(&conn, &key, "reserved:reader");
        ledger::settle(&conn, &key, &frame(), "ok", None, ledger::now_ms()).unwrap();
        conn.execute(
            "UPDATE call_ledger SET event_id=?1, outcome=?2 WHERE carrier=?3 AND call_key=?4",
            params![event_id, outcome, key.carrier, key.call_key],
        )
        .unwrap();
        let mut publisher = RecordingPublisher::default();
        drain(&conn, &mut DeliveryState::default(), &mut publisher).await;
        assert_eq!(publisher.attempts.len(), 1, "{case}");
        let (reader, msg_id, body) = &publisher.attempts[0];
        assert_eq!(
            body.as_slice(),
            fixture(&format!("{case}/payload.json")).as_bytes(),
            "{case}"
        );
        assert_eq!(
            msg_id,
            fixture(&format!("{case}/nats-msg-id.txt")).trim_end_matches('\n'),
            "{case}"
        );
        let subject = AccountNames::derive("box_test")
            .unwrap()
            .event_subject_with_reader(
                "provider",
                LATE_RESULT_READY_EVENT,
                LATE_RESULT_READY_VERSION as u32,
                reader,
            )
            .unwrap();
        assert_eq!(
            subject.strip_prefix("ck.box_test.").unwrap(),
            fixture(&format!("{case}/subject.txt")).trim_end_matches('\n'),
            "{case}"
        );
    }
}

#[tokio::test]
async fn late_result_bus_committed_entry_is_sent_once_with_only_public_fields() {
    let (_dir, conn) = database();
    let key = key("call-42");
    admit(&conn, &key, "reserved:broca");
    let mut state = DeliveryState::default();
    let mut publisher = RecordingPublisher::default();
    drain(&conn, &mut state, &mut publisher).await;
    assert!(publisher.attempts.is_empty());
    ledger::settle(&conn, &key, &frame(), "ok", None, ledger::now_ms()).unwrap();
    let event_id = ledger::get(&conn, &key).unwrap().unwrap().event_id;
    state.hint();
    drain(&conn, &mut state, &mut publisher).await;
    state.hint();
    drain(&conn, &mut state, &mut publisher).await;
    assert_eq!(publisher.attempts.len(), 1);
    let (reader, msg_id, body) = &publisher.attempts[0];
    assert_eq!(reader, "broca");
    assert_eq!(msg_id, &format!("{event_id}:broca"));
    assert_eq!(
        serde_json::from_slice::<Value>(body).unwrap(),
        json!({"call_key":"call-42","outcome":"ok"})
    );
    let names = AccountNames::derive("box_test").unwrap();
    assert_eq!(
        names
            .event_subject_with_reader(
                "aft",
                LATE_RESULT_READY_EVENT,
                LATE_RESULT_READY_VERSION as u32,
                reader
            )
            .unwrap(),
        "ck.box_test.event.aft.late_result_ready.v1.broca"
    );
    let page = ledger::late_results(
        &conn,
        "reserved:broca",
        ledger::provider_incarnation(),
        None,
        None,
        ledger::now_ms(),
        1024 * 1024,
    )
    .unwrap()
    .unwrap();
    let feed: Value = serde_json::from_slice(&page).unwrap();
    assert_eq!(feed["entries"][0]["call_key"], "call-42");
    assert_eq!(
        ledger::get(&conn, &key)
            .unwrap()
            .unwrap()
            .outcome
            .as_deref(),
        Some("ok")
    );
}

#[tokio::test]
async fn late_result_bus_never_reads_uncommitted_or_failed_settlement() {
    let (dir, writer) = database();
    let reader = crate::db::open_readonly(&dir.path().join("aft.db")).unwrap();
    let key = key("commit-fails");
    admit(&writer, &key, "reserved:broca");
    let mut publisher = RecordingPublisher::default();
    let mut state = DeliveryState::default();
    writer.execute_batch("BEGIN IMMEDIATE; UPDATE call_ledger SET state='Settled', late_entry='{}', outcome='ok' WHERE call_key='commit-fails';").unwrap();
    drain(&reader, &mut state, &mut publisher).await;
    assert!(
        publisher.attempts.is_empty(),
        "uncommitted settlement is not visible"
    );
    writer.execute_batch("ROLLBACK;
        CREATE TABLE bus_commit_parent(id INTEGER PRIMARY KEY);
        CREATE TABLE bus_commit_child(id INTEGER REFERENCES bus_commit_parent(id) DEFERRABLE INITIALLY DEFERRED);
        CREATE TRIGGER fail_bus_commit AFTER UPDATE OF state ON call_ledger WHEN NEW.state='Settled'
        BEGIN INSERT INTO bus_commit_child VALUES(1); END;").unwrap();
    assert!(
        ledger::settle(&writer, &key, &frame(), "ok", None, ledger::now_ms()).is_err(),
        "deferred foreign key fails COMMIT"
    );
    assert_eq!(
        ledger::get(&writer, &key).unwrap().unwrap().state,
        ledger::State::Prepared
    );
    state.hint();
    drain(&reader, &mut state, &mut publisher).await;
    assert!(publisher.attempts.is_empty());
    writer
        .execute_batch("DROP TRIGGER fail_bus_commit;")
        .unwrap();
    ledger::settle(&writer, &key, &frame(), "ok", None, ledger::now_ms()).unwrap();
    state.hint();
    drain(&reader, &mut state, &mut publisher).await;
    assert_eq!(publisher.attempts.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn late_result_bus_dropped_publish_retries_and_restart_replays_stable_id() {
    let (_dir, conn) = database();
    let key = key("retry");
    admit(&conn, &key, "reserved:prefrontal-core");
    ledger::settle(
        &conn,
        &key,
        &frame(),
        "outcome_unknown",
        None,
        ledger::now_ms(),
    )
    .unwrap();
    let mut state = DeliveryState::default();
    let mut publisher = RecordingPublisher {
        drop_next: true,
        ..Default::default()
    };
    drain(&conn, &mut state, &mut publisher).await;
    assert_eq!(publisher.attempts.len(), 1);
    assert_eq!(state.pending.len(), 1);
    assert!(state.acknowledged.is_empty());
    tokio::time::advance(Duration::from_secs(1)).await;
    retry_due(&conn, &mut state, &mut publisher).await;
    assert_eq!(publisher.attempts.len(), 2);
    assert!(state.pending.is_empty());
    assert_eq!(publisher.attempts[0], publisher.attempts[1]);
    // A restart has no bus ack set. The retained feed reconstructs the wake
    // even when no new settlement notification arrives.
    let mut restarted = DeliveryState::default();
    drain(&conn, &mut restarted, &mut publisher).await;
    assert_eq!(publisher.attempts.len(), 3);
    assert_eq!(publisher.attempts[1], publisher.attempts[2]);
    assert_eq!(
        serde_json::from_slice::<Value>(&publisher.attempts[2].2).unwrap()["outcome"],
        "outcome_unknown"
    );
}

#[tokio::test(start_paused = true)]
async fn late_result_bus_retry_set_uses_backoff_without_ledger_rescans() {
    let (_dir, conn) = database();
    let key = key("no-hint");
    admit(&conn, &key, "reserved:thalamus");
    ledger::settle(&conn, &key, &frame(), "ok", None, ledger::now_ms()).unwrap();
    let mut state = DeliveryState::default();
    let mut publisher = RecordingPublisher {
        drop_next: true,
        ..Default::default()
    };
    drain(&conn, &mut state, &mut publisher).await;
    let settled = Notify::new();
    for seconds in [1, 2, 4, 8, 16, 32, 60, 60] {
        let now = tokio::time::Instant::now();
        assert_eq!(
            state.next_retry().unwrap() - now,
            Duration::from_secs(seconds)
        );
        tokio::time::advance(Duration::from_secs(seconds) - Duration::from_millis(1)).await;
        assert!(state.due().is_empty());
        tokio::time::advance(Duration::from_millis(1)).await;
        publisher.drop_next = true;
        retry_due(&conn, &mut state, &mut publisher).await;
    }
    tokio::time::advance(Duration::from_secs(60)).await;
    retry_due(&conn, &mut state, &mut publisher).await;
    assert!(state.pending.is_empty());
    assert_eq!(publisher.attempts.len(), 10);
    assert!(publisher
        .attempts
        .iter()
        .all(|attempt| attempt == &publisher.attempts[0]));
    // Idle waits do not request a scan, including beyond a minute.
    for _ in 0..61 {
        assert!(!wait_for_work(&settled, None).await);
    }
    settled.notify_one();
    assert!(wait_for_work(&settled, None).await);
}

#[tokio::test(start_paused = true)]
async fn late_result_bus_stalled_publish_cannot_delay_settlement_reply() {
    let (_dir, conn) = database();
    let first = key("slow-bus");
    admit(&conn, &first, "reserved:broca");
    ledger::settle(&conn, &first, &frame(), "ok", None, ledger::now_ms()).unwrap();
    let mut state = DeliveryState::default();
    let mut publisher = RecordingPublisher {
        stall: true,
        ..Default::default()
    };
    let pending = state.deliver(
        &mut publisher,
        scan(&conn, Cursor::default(), state.capacity()).unwrap(),
    );
    tokio::pin!(pending);
    tokio::select! {
        biased;
        result = &mut pending => panic!("bus should still be waiting: {result:?}"),
        _ = tokio::task::yield_now() => {},
    }
    let second = key("reply-does-not-wait");
    admit(&conn, &second, "reserved:broca");
    let before = tokio::time::Instant::now();
    assert!(ledger::settle(&conn, &second, &frame(), "ok", None, ledger::now_ms()).unwrap());
    assert_eq!(tokio::time::Instant::now(), before);
    assert_eq!(
        ledger::get(&conn, &second).unwrap().unwrap().state,
        ledger::State::Settled
    );
    assert!(!pending.await);
}

#[tokio::test]
async fn late_result_bus_hint_visits_only_new_rows_after_fifty_thousand_settlements() {
    let (_dir, conn) = database();
    conn.execute_batch(
        "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<50000)
        INSERT INTO call_ledger(carrier,call_key,digest,state,event_id,settled_at,outcome)
        SELECT 'reserved:broca','old-'||i,'digest','Settled','old-event-'||i,1000,'ok' FROM n;",
    )
    .unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM call_ledger", [], |row| row
            .get::<_, usize>(0))
            .unwrap(),
        50_000
    );
    let mut state = DeliveryState::default();
    let mut publisher = RecordingPublisher::default();
    let baseline = key("last-cursor");
    admit(&conn, &baseline, "reserved:broca");
    ledger::settle(&conn, &baseline, &frame(), "ok", None, 20_000).unwrap();
    // Bootstrap uses index maxima and feed seeks, not the 50,000 historical rows.
    drain(&conn, &mut state, &mut publisher).await;
    assert_eq!(state.high_water.settled_at, 20_000);
    ledger::ack(&conn, "reserved:broca", 1).unwrap();
    publisher.attempts.clear();
    let new = key("new-settlement");
    admit(&conn, &new, "reserved:broca");
    ledger::settle(&conn, &new, &frame(), "ok", None, 40_000).unwrap();
    let hint = Notify::new();
    hint.notify_one();
    assert!(wait_for_work(&hint, None).await);
    state.hint();
    let visited = drain(&conn, &mut state, &mut publisher).await;
    assert_eq!(visited, 1, "a hint must not walk the retained ledger");
    assert_eq!(publisher.attempts.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&publisher.attempts[0].2).unwrap()["call_key"],
        "new-settlement"
    );
}

#[test]
fn late_result_bus_errors_are_local_codes_not_peer_text() {
    for error in [
        cortexkit_bus_client::Error::Refused("secret /peer/path".into()),
        cortexkit_bus_client::Error::Transport("credential JWT or peer text".into()),
        cortexkit_bus_client::Error::Nats("private response".into()),
    ] {
        let code = error_code(&error);
        assert!(code.starts_with("bus_"));
        assert!(code.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'));
    }
}

#[tokio::test]
async fn late_result_bus_restart_does_not_republish_feed_acknowledged_entries() {
    let (_dir, conn) = database();
    let acknowledged = key("acknowledged");
    let retained = key("retained");
    let marked = key("marked");
    for key in [&acknowledged, &retained, &marked] {
        admit(&conn, key, "reserved:broca");
        ledger::settle(&conn, key, &frame(), "ok", None, 20_000).unwrap();
    }
    conn.execute_batch("CREATE TEMP TABLE saved_acknowledged AS SELECT * FROM call_ledger WHERE call_key='acknowledged';").unwrap();
    assert_eq!(ledger::ack(&conn, "reserved:broca", 1).unwrap(), 1);
    assert!(
        ledger::get(&conn, &acknowledged).unwrap().is_none(),
        "terminal feed ack deletes its row"
    );
    // Also cover retained marker forms defensively: the feed cursor excludes
    // a restored row below through_seq, and acked=1 excludes a marked row.
    conn.execute_batch(
        "INSERT INTO call_ledger SELECT * FROM saved_acknowledged;
        UPDATE call_ledger SET acked=1 WHERE call_key='marked';",
    )
    .unwrap();
    assert!(ledger::get(&conn, &acknowledged)
        .unwrap()
        .unwrap()
        .late_entry
        .is_some());
    let mut state = DeliveryState::default();
    let mut publisher = RecordingPublisher::default();
    drain(&conn, &mut state, &mut publisher).await;
    assert_eq!(publisher.attempts.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&publisher.attempts[0].2).unwrap()["call_key"],
        "retained"
    );
    state.hint();
    drain(&conn, &mut state, &mut publisher).await;
    assert_eq!(publisher.attempts.len(), 1);
}

#[tokio::test]
async fn late_result_bus_overlap_catches_later_commit_with_equal_timestamp_and_older_rowid_once() {
    let (_dir, conn) = database();
    let older = key("older-admission");
    let newer = key("newer-admission");
    admit(&conn, &older, "reserved:broca");
    admit(&conn, &newer, "reserved:broca");
    ledger::settle(&conn, &newer, &frame(), "ok", None, 20_000).unwrap();
    let mut state = DeliveryState::default();
    let mut publisher = RecordingPublisher::default();
    drain(&conn, &mut state, &mut publisher).await;
    let newer_cursor = state.high_water;
    assert_eq!(publisher.attempts.len(), 1);
    ledger::settle(&conn, &older, &frame(), "ok", None, 20_000).unwrap();
    let older_rowid: i64 = conn
        .query_row(
            "SELECT rowid FROM call_ledger WHERE call_key=?1",
            [&older.call_key],
            |row| row.get(0),
        )
        .unwrap();
    assert!(older_rowid < newer_cursor.rowid);
    assert!(
        scan(&conn, newer_cursor, SCAN_BATCH)
            .unwrap()
            .notices
            .is_empty(),
        "a strict cursor would miss the equal-time older admission"
    );
    state.hint();
    drain(&conn, &mut state, &mut publisher).await;
    state.hint();
    drain(&conn, &mut state, &mut publisher).await;
    assert_eq!(publisher.attempts.len(), 2);
    assert_eq!(
        serde_json::from_slice::<Value>(&publisher.attempts[1].2).unwrap()["call_key"],
        "older-admission"
    );
    let latest = key("latest");
    admit(&conn, &latest, "reserved:broca");
    ledger::settle(&conn, &latest, &frame(), "ok", None, 40_001).unwrap();
    state.hint();
    drain(&conn, &mut state, &mut publisher).await;
    assert_eq!(
        state.acknowledged.len(),
        1,
        "old overlap acknowledgements are pruned"
    );
}

#[tokio::test(start_paused = true)]
async fn late_result_bus_retry_cap_keeps_overflow_in_the_ledger() {
    let (_dir, conn) = database();
    conn.execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<1001)
        INSERT INTO call_ledger(carrier,call_key,digest,state,event_id,settled_at,outcome,owner,scope_ref,scope_epoch,custodian,seq,late_entry)
        SELECT 'reserved:broca','queued-'||i,'digest','Settled','queued-event-'||i,20000,'ok',
            CASE WHEN i<=1000 THEN 'reserved:broca' ELSE 'reserved:thalamus' END,'scope',1,
            CASE WHEN i<=1000 THEN 'reserved:broca' ELSE 'reserved:thalamus' END,i,'{}' FROM n;
        INSERT INTO call_ledger_ack(custodian,last_seq) VALUES ('reserved:broca',1000),('reserved:thalamus',1001);").unwrap();
    let mut state = DeliveryState {
        position: Position::Live(Cursor::default()),
        ..Default::default()
    };
    let mut publisher = RecordingPublisher {
        fail: true,
        ..Default::default()
    };
    assert_eq!(
        drain(&conn, &mut state, &mut publisher).await,
        UNACKED_RESULTS_MAX
    );
    assert_eq!(state.pending.len(), UNACKED_RESULTS_MAX);
    assert_eq!(publisher.attempts.len(), UNACKED_RESULTS_MAX);
    assert_eq!(state.capacity(), 0);
    let Position::Live(cursor) = state.position else {
        panic!("expected a live cursor")
    };
    assert_eq!(cursor.rowid, 1000);
    publisher.fail = false;
    publisher.attempts.clear();
    tokio::time::advance(Duration::from_secs(1)).await;
    let retry = state.due().remove(0);
    state.retry(&mut publisher, vec![(retry, true)]).await;
    assert_eq!(drain(&conn, &mut state, &mut publisher).await, 1);
    assert_eq!(publisher.attempts.len(), 2);
    assert_eq!(
        serde_json::from_slice::<Value>(&publisher.attempts[1].2).unwrap()["call_key"],
        "queued-1001"
    );
}

#[tokio::test(start_paused = true)]
async fn late_result_bus_retry_drops_entries_acknowledged_by_the_feed() {
    let (_dir, conn) = database();
    let key = key("already-read");
    admit(&conn, &key, "reserved:broca");
    ledger::settle(&conn, &key, &frame(), "ok", None, 20_000).unwrap();
    let mut state = DeliveryState::default();
    let mut publisher = RecordingPublisher {
        fail: true,
        ..Default::default()
    };
    drain(&conn, &mut state, &mut publisher).await;
    assert_eq!(state.pending.len(), 1);
    ledger::ack(&conn, "reserved:broca", 1).unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    retry_due(&conn, &mut state, &mut publisher).await;
    assert!(state.pending.is_empty());
    assert_eq!(publisher.attempts.len(), 1);
}

#[test]
fn late_result_bus_live_scan_uses_existing_retention_index() {
    let (_dir, conn) = database();
    let plan: Vec<String> = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {LIVE_SCAN_SQL}"))
        .unwrap()
        .query_map(params![10_000, 1, SCAN_BATCH], |row| row.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|line| line.contains("SEARCH call_ledger USING INDEX idx_call_ledger_retention")),
        "{plan:?}"
    );
    assert!(
        !plan
            .iter()
            .any(|line| line.contains("SCAN call_ledger") || line.contains("TEMP B-TREE")),
        "{plan:?}"
    );
}
