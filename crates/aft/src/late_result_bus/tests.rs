use super::*;
use crate::db::call_ledger as ledger;
use cortexkit_bus_naming::AccountNames;
use serde_json::{json, Value};
use subc_protocol::FrameType;

#[derive(Default)]
struct RecordingPublisher {
    attempts: Vec<(String, String, Vec<u8>)>,
    drop_next: bool,
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
        if std::mem::take(&mut self.drop_next) {
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

#[tokio::test]
async fn late_result_bus_committed_entry_is_sent_once_with_only_public_fields() {
    let (_dir, conn) = database();
    let key = key("call-42");
    admit(&conn, &key, "reserved:broca");
    let mut state = DeliveryState::default();
    let mut publisher = RecordingPublisher::default();
    state
        .deliver(&mut publisher, scan(&conn, 0).unwrap())
        .await
        .unwrap();
    assert!(publisher.attempts.is_empty());
    ledger::settle(&conn, &key, &frame(), "ok", None, ledger::now_ms()).unwrap();
    let event_id = ledger::get(&conn, &key).unwrap().unwrap().event_id;
    state
        .deliver(&mut publisher, scan(&conn, 0).unwrap())
        .await
        .unwrap();
    state
        .deliver(&mut publisher, scan(&conn, 0).unwrap())
        .await
        .unwrap();
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
            .event_subject_with_reader("aft", EVENT, 1, reader)
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
    state
        .deliver(&mut publisher, scan(&reader, 0).unwrap())
        .await
        .unwrap();
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
    state
        .deliver(&mut publisher, scan(&reader, 0).unwrap())
        .await
        .unwrap();
    assert!(publisher.attempts.is_empty());
    writer
        .execute_batch("DROP TRIGGER fail_bus_commit;")
        .unwrap();
    ledger::settle(&writer, &key, &frame(), "ok", None, ledger::now_ms()).unwrap();
    state
        .deliver(&mut publisher, scan(&reader, 0).unwrap())
        .await
        .unwrap();
    assert_eq!(publisher.attempts.len(), 1);
}

#[tokio::test]
async fn late_result_bus_dropped_publish_retries_and_restart_recheck_replays_stable_id() {
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
    assert_eq!(
        state.deliver(&mut publisher, scan(&conn, 0).unwrap()).await,
        Err("bus_publish")
    );
    assert!(state.acknowledged.is_empty());
    state
        .deliver(&mut publisher, scan(&conn, 0).unwrap())
        .await
        .unwrap();
    assert_eq!(publisher.attempts.len(), 2);
    assert_eq!(publisher.attempts[0], publisher.attempts[1]);
    // A restart has no bus ack set. The retained feed, not an in-memory queue,
    // reconstructs the wake even when no new settlement notification arrives.
    let mut restarted = DeliveryState::default();
    restarted
        .deliver(&mut publisher, scan(&conn, 0).unwrap())
        .await
        .unwrap();
    assert_eq!(publisher.attempts[1], publisher.attempts[2]);
    assert_eq!(
        serde_json::from_slice::<Value>(&publisher.attempts[2].2).unwrap()["outcome"],
        "outcome_unknown"
    );
    ledger::ack(&conn, "reserved:prefrontal-core", u64::MAX / 2).unwrap();
    restarted
        .deliver(&mut publisher, scan(&conn, 0).unwrap())
        .await
        .unwrap();
    assert!(restarted.acknowledged.is_empty());
}

#[tokio::test(start_paused = true)]
async fn late_result_bus_recheck_without_hint_republishes_unacknowledged_entry() {
    let (_dir, conn) = database();
    let key = key("no-hint");
    admit(&conn, &key, "reserved:thalamus");
    ledger::settle(&conn, &key, &frame(), "ok", None, ledger::now_ms()).unwrap();
    let mut state = DeliveryState::default();
    let mut publisher = RecordingPublisher {
        drop_next: true,
        ..Default::default()
    };
    assert!(state
        .deliver(&mut publisher, scan(&conn, 0).unwrap())
        .await
        .is_err());
    let settled = Notify::new();
    let now = tokio::time::Instant::now();
    let mut next_check = now + RECHECK;
    while tokio::time::Instant::now() < next_check {
        wait_for_work(&settled, &mut next_check).await;
    }
    assert_eq!(tokio::time::Instant::now() - now, Duration::from_secs(60));
    state
        .deliver(&mut publisher, scan(&conn, 0).unwrap())
        .await
        .unwrap();
    assert_eq!(publisher.attempts.len(), 2);
    assert_eq!(publisher.attempts[0], publisher.attempts[1]);
    let schedule: Vec<_> =
        std::iter::successors(Some(Duration::from_secs(1)), |d| Some(backoff(*d)))
            .take(8)
            .map(|d| d.as_secs())
            .collect();
    assert_eq!(schedule, [1, 2, 4, 8, 16, 32, 60, 60]);
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
    let pending = state.deliver(&mut publisher, scan(&conn, 0).unwrap());
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
    assert_eq!(pending.await, Err("bus_publish_timeout"));
}

#[tokio::test]
async fn late_result_bus_scan_bounds_rows_before_filtering_and_preserves_continuation() {
    let (_dir, conn) = database();
    for i in 0..SCAN_BATCH {
        admit(&conn, &key(&format!("pending-{i}")), "reserved:broca");
    }
    let last = key("last");
    admit(&conn, &last, "reserved:broca");
    ledger::settle(&conn, &last, &frame(), "ok", None, ledger::now_ms()).unwrap();
    let batch = scan(&conn, 0).unwrap();
    assert_eq!(batch.after, SCAN_BATCH as i64);
    assert!(batch.full);
    assert!(batch.notices.is_empty());
    let mut state = DeliveryState::default();
    let mut publisher = RecordingPublisher::default();
    assert!(state.deliver(&mut publisher, batch).await.unwrap());
    assert!(!state
        .deliver(&mut publisher, scan(&conn, state.after).unwrap())
        .await
        .unwrap());
    assert_eq!(publisher.attempts.len(), 1);
    assert_eq!(state.after, 0);
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
