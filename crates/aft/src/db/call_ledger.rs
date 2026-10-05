//! Durable admission and settlement for keyed tool-provider calls.
//!
//! All operations use the actor's existing connection. State changes and late
//! entries share a transaction: a crash cannot publish half a settlement.
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subc_protocol::FrameType;

pub const RETENTION_MS: i64 = 24 * 60 * 60 * 1000;
pub const SWEEP_BATCH: usize = 64;

/// One process-wide cursor identity, deliberately never persisted in aft.db.
/// Reconnecting or binding another project must not look like a restart.
pub fn provider_incarnation() -> &'static str {
    static INCARNATION: std::sync::LazyLock<String> = std::sync::LazyLock::new(new_event_id);
    INCARNATION.as_str()
}
pub(super) const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS call_ledger (
    carrier TEXT NOT NULL,
    call_key TEXT NOT NULL,
    digest TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('Prepared','Authorized','DispatchStarted','Settled')),
    owner TEXT,
    scope_ref TEXT,
    scope_epoch INTEGER,
    custodian TEXT,
    task_id TEXT,
    withdraw_answer BLOB,
    frame_type TEXT,
    frame_body BLOB,
    outcome TEXT,
    event_id TEXT NOT NULL,
    settled_at INTEGER,
    seq INTEGER,
    acked INTEGER NOT NULL DEFAULT 0,
    late_entry TEXT,
    recovered INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(carrier, call_key),
    CHECK ((owner IS NULL AND scope_ref IS NULL AND scope_epoch IS NULL AND custodian IS NULL)
        OR (owner IS NOT NULL AND scope_ref IS NOT NULL AND scope_epoch IS NOT NULL AND custodian IS NOT NULL AND custodian = owner))
);
CREATE INDEX IF NOT EXISTS idx_call_ledger_retention ON call_ledger(state, settled_at);
CREATE INDEX IF NOT EXISTS idx_call_ledger_custodian ON call_ledger(custodian, seq);
CREATE INDEX IF NOT EXISTS idx_call_ledger_task ON call_ledger(task_id);
CREATE TABLE IF NOT EXISTS call_ledger_ack (
    custodian TEXT PRIMARY KEY,
    through_seq INTEGER NOT NULL DEFAULT 0,
    last_seq INTEGER NOT NULL DEFAULT 0
);
"#;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub carrier: String,
    pub call_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeIdentity {
    pub owner: String,
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub scope_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Prepared,
    Authorized,
    DispatchStarted,
    Settled,
}
impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "Prepared",
            Self::Authorized => "Authorized",
            Self::DispatchStarted => "DispatchStarted",
            Self::Settled => "Settled",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecordedFrame {
    pub ty: FrameType,
    pub body: Vec<u8>,
}
impl RecordedFrame {
    fn type_name(&self) -> &'static str {
        match self.ty {
            FrameType::Response => "Response",
            FrameType::StreamEnd => "StreamEnd",
            _ => "Error",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub key: Key,
    pub digest: String,
    pub state: State,
    pub scope: Option<ScopeIdentity>,
    pub task_id: Option<String>,
    pub withdraw_answer: Option<Vec<u8>>,
    pub frame: Option<RecordedFrame>,
    pub outcome: Option<String>,
    pub event_id: String,
    pub settled_at: Option<i64>,
    pub seq: Option<u64>,
    pub late_entry: Option<String>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub fn get(conn: &Connection, key: &Key) -> rusqlite::Result<Option<Row>> {
    conn.query_row(
        "SELECT digest,state,owner,scope_ref,scope_epoch,task_id,withdraw_answer,frame_type,frame_body,outcome,event_id,settled_at,seq,late_entry
         FROM call_ledger WHERE carrier=?1 AND call_key=?2",
        params![key.carrier, key.call_key],
        |r| {
            let owner: Option<String> = r.get(2)?;
            let ty: Option<String> = r.get(7)?;
            let body: Option<Vec<u8>> = r.get(8)?;
            Ok(Row {
                key: key.clone(), digest: r.get(0)?,
                state: match r.get::<_, String>(1)?.as_str() {
                    "Prepared" => State::Prepared, "Authorized" => State::Authorized,
                    "DispatchStarted" => State::DispatchStarted, _ => State::Settled,
                },
                scope: owner.map(|owner| Ok::<_, rusqlite::Error>(ScopeIdentity { owner, scope_ref: r.get(3)?, scope_epoch: r.get(4)? })).transpose()?,
                task_id: r.get(5)?, withdraw_answer: r.get(6)?,
                frame: ty.zip(body).map(|(ty, body)| RecordedFrame { ty: match ty.as_str() { "Response" => FrameType::Response, "StreamEnd" => FrameType::StreamEnd, _ => FrameType::Error }, body }),
                outcome: r.get(9)?, event_id: r.get(10)?, settled_at: r.get(11)?, seq: r.get(12)?, late_entry: r.get(13)?,
            })
        },
    ).optional()
}

pub enum Admission {
    New,
    Repeat(Row),
    Conflict,
}

pub fn admit(
    conn: &Connection,
    key: &Key,
    digest: &str,
    scope: Option<&ScopeIdentity>,
    state: State,
) -> rusqlite::Result<Admission> {
    let tx = conn.unchecked_transaction()?;
    if let Some(row) = get(&tx, key)? {
        return Ok(if row.digest == digest && row.scope.as_ref() == scope {
            Admission::Repeat(row)
        } else {
            Admission::Conflict
        });
    }
    tx.execute("INSERT INTO call_ledger (carrier,call_key,digest,state,owner,scope_ref,scope_epoch,custodian,event_id)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?5,?8)",
        params![key.carrier,key.call_key,digest,state.as_str(),scope.map(|s| &s.owner),scope.map(|s| &s.scope_ref),scope.map(|s| s.scope_epoch),new_event_id()])?;
    tx.commit()?;
    crash_point(state);
    Ok(Admission::New)
}

pub fn transition(conn: &Connection, key: &Key, from: State, to: State) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE call_ledger SET state=?3 WHERE carrier=?1 AND call_key=?2 AND state=?4",
        params![key.carrier, key.call_key, to.as_str(), from.as_str()],
    )? == 1;
    if changed {
        crash_point(to);
    }
    Ok(changed)
}

pub fn not_started(key: &Key, reason: &str) -> RecordedFrame {
    RecordedFrame { ty: FrameType::Error, body: serde_json::to_vec(&json!({"code":"not_started","message":"call was not started","detail":{"call_key":key.call_key,"reason":reason}})).unwrap() }
}
pub fn not_retained(key: &Key, outcome: &str) -> RecordedFrame {
    RecordedFrame { ty: FrameType::Error, body: serde_json::to_vec(&json!({"code":"result_not_retained","message":"call result is no longer retained","detail":{"call_key":key.call_key,"outcome":outcome}})).unwrap() }
}

/// Outcome is a protocol determination, not a guess from formatted tool text.
pub fn outcome(frame: &RecordedFrame) -> String {
    if frame.ty == FrameType::StreamEnd {
        return "ok".into();
    }
    let value: Value = serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
    if frame.ty == FrameType::Error {
        return value["code"].as_str().unwrap_or("unknown").into();
    }
    // Tool responses carry the native Response in structuredContent.
    let response = value.get("structuredContent").unwrap_or(&value);
    if response["success"] == true || value["isError"] == false {
        "ok".into()
    } else {
        response["code"].as_str().unwrap_or("unknown").into()
    }
}

/// Untrusted routes deliberately omit structuredContent. Preserve the native
/// error code on the executor before formatting removes that information.
pub fn note_native_outcome(
    conn: &Connection,
    key: &Key,
    response: &crate::protocol::Response,
) -> rusqlite::Result<()> {
    let code = if response.success {
        "ok"
    } else {
        response.data["code"].as_str().unwrap_or("unknown")
    };
    conn.execute("UPDATE call_ledger SET outcome=?3 WHERE carrier=?1 AND call_key=?2 AND state='DispatchStarted'",params![key.carrier,key.call_key,code])?;
    Ok(())
}

pub fn settle(
    conn: &Connection,
    key: &Key,
    frame: &RecordedFrame,
    outcome: &str,
    reason: Option<&str>,
    now: i64,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let Some(row) = get(&tx, key)? else {
        return Ok(false);
    };
    if row.state == State::Settled {
        return Ok(false);
    }
    let mut seq = None;
    let mut entry = None;
    if let Some(scope) = &row.scope {
        tx.execute("INSERT INTO call_ledger_ack(custodian,last_seq) VALUES (?1,1) ON CONFLICT(custodian) DO UPDATE SET last_seq=last_seq+1", [&scope.owner])?;
        seq = Some(tx.query_row(
            "SELECT last_seq FROM call_ledger_ack WHERE custodian=?1",
            [&scope.owner],
            |r| r.get::<_, u64>(0),
        )?);
        let mut value = json!({"kind":if reason.is_some() {"not_started"} else {"result"},"owner":scope.owner,"ref":scope.scope_ref,"scope_epoch":scope.scope_epoch,"custodian":scope.owner,"call_key":key.call_key,"event_id":row.event_id,"settled_at":now,"reduced":outcome == "unknown"});
        if let Some(reason) = reason {
            value["reason"] = json!(reason);
        } else if outcome == "unknown" {
            value["outcome"] = json!(outcome);
        } else {
            value["result"] = serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
        }
        entry = Some(value.to_string());
    }
    tx.execute("UPDATE call_ledger SET state='Settled',frame_type=?3,frame_body=?4,outcome=?5,settled_at=?6,seq=?7,late_entry=?8 WHERE carrier=?1 AND call_key=?2 AND state=?9",
        params![key.carrier,key.call_key,frame.type_name(),frame.body,outcome,now,seq,entry,row.state.as_str()])?;
    tx.commit()?;
    Ok(true)
}

/// Expiry drops only the payload. Identity, outcome and any frozen withdrawal
/// answer remain until the entry is acknowledged or dropped.
pub fn reduce_expired(conn: &Connection, key: &Key, now: i64) -> rusqlite::Result<()> {
    let Some(row) = get(conn, key)? else {
        return Ok(());
    };
    if row.state != State::Settled || row.settled_at.is_none_or(|at| at + RETENTION_MS > now) {
        return Ok(());
    }
    let frame = not_retained(key, row.outcome.as_deref().unwrap_or("unknown"));
    let entry = row.late_entry.map(|entry| {
        let mut v: Value = serde_json::from_str(&entry).unwrap();
        v["kind"] = json!("expired");
        v["reduced"] = json!(true);
        v.as_object_mut().unwrap().remove("result");
        v.as_object_mut().unwrap().remove("reason");
        v.to_string()
    });
    conn.execute("UPDATE call_ledger SET frame_type='Error',frame_body=?3,late_entry=?4 WHERE carrier=?1 AND call_key=?2", params![key.carrier,key.call_key,frame.body,entry])?;
    Ok(())
}

/// A SQL LIMIT bounds the work before collecting any rows.
pub fn sweep(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare("SELECT carrier,call_key FROM call_ledger WHERE state='Settled' AND (acked=1 OR (settled_at<=?1 AND (owner IS NULL OR json_extract(late_entry,'$.kind')!='expired'))) ORDER BY settled_at,carrier,call_key LIMIT ?2")?;
    let keys = stmt
        .query_map(params![now - RETENTION_MS, SWEEP_BATCH], |r| {
            Ok(Key {
                carrier: r.get(0)?,
                call_key: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for key in &keys {
        conn.execute("DELETE FROM call_ledger WHERE carrier=?1 AND call_key=?2 AND (acked=1 OR owner IS NULL)", params![key.carrier,key.call_key])?;
        reduce_expired(conn, &key, now)?;
    }
    Ok(keys.len())
}

pub fn drop_entry(conn: &Connection, key: &Key) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM call_ledger WHERE carrier=?1 AND call_key=?2 AND state='Settled'",
        params![key.carrier, key.call_key],
    )? == 1)
}
pub fn ack(conn: &Connection, custodian: &str, through: u64) -> rusqlite::Result<usize> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE call_ledger_ack SET through_seq=MAX(through_seq,?2) WHERE custodian=?1",
        params![custodian, through],
    )?;
    let deleted = tx.execute(
        "DELETE FROM call_ledger WHERE custodian=?1 AND seq<=?2",
        params![custodian, through],
    )?;
    tx.commit()?;
    Ok(deleted)
}

/// Called once when a process installs its database, before admitting calls.
/// A dispatch with a durable task is observed, never re-sent.
pub fn recover(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt =
        conn.prepare("SELECT carrier,call_key FROM call_ledger WHERE state!='Settled'")?;
    let keys = stmt
        .query_map([], |r| {
            Ok(Key {
                carrier: r.get(0)?,
                call_key: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for key in keys {
        let row = get(conn, &key)?.unwrap();
        if matches!(row.state, State::Prepared | State::Authorized) {
            settle(
                conn,
                &key,
                &not_started(&key, "restart_before_dispatch"),
                "not_started",
                Some("restart_before_dispatch"),
                now_ms(),
            )?;
        } else {
            // Keys become reusable after their ledger row is deleted. Only
            // the task linked to this admission is evidence of its execution;
            // a previous task carrying the same consumer key is not.
            let task: Option<String> = conn.query_row("SELECT task_id FROM bash_tasks WHERE task_id=?1 AND json_extract(metadata,'$.call_key.requester')=?2 AND json_extract(metadata,'$.call_key.key')=?3 LIMIT 1", params![row.task_id,key.carrier,key.call_key], |r| r.get(0)).optional()?;
            if let Some(task) = task {
                conn.execute("UPDATE call_ledger SET task_id=?3,recovered=1 WHERE carrier=?1 AND call_key=?2", params![key.carrier,key.call_key,task])?;
            } else {
                settle(
                    conn,
                    &key,
                    &not_retained(&key, "unknown"),
                    "unknown",
                    None,
                    now_ms(),
                )?;
            }
        }
    }
    while sweep(conn, now_ms())? == SWEEP_BATCH {}
    Ok(())
}

/// Project actors share the tracked connection. Attaching a second registry
/// to that connection is not a provider restart. Weak references also avoid
/// keeping closed databases alive or confusing a reused allocation address.
pub fn recover_pool_once(
    pool: &std::sync::Arc<std::sync::Mutex<super::TrackedConnection>>,
) -> rusqlite::Result<()> {
    type Pool = std::sync::Mutex<super::TrackedConnection>;
    static RECOVERED: std::sync::LazyLock<std::sync::Mutex<Vec<std::sync::Weak<Pool>>>> =
        std::sync::LazyLock::new(Default::default);
    let mut recovered = RECOVERED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    recovered.retain(|old| old.strong_count() != 0);
    if recovered.iter().any(|old| {
        old.upgrade()
            .is_some_and(|old| std::sync::Arc::ptr_eq(&old, pool))
    }) {
        return Ok(());
    }
    let conn = pool.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
    recover(&conn)?;
    recovered.push(std::sync::Arc::downgrade(pool));
    Ok(())
}

/// Link the task born during this admitted v1 dispatch, never a legacy task
/// or a later snapshot of an older task sharing a reusable consumer key.
pub fn link_task(conn: &Connection, key: &Key, task_id: &str) -> rusqlite::Result<()> {
    conn.execute("UPDATE call_ledger SET task_id=?3 WHERE carrier=?1 AND call_key=?2 AND state='DispatchStarted' AND task_id IS NULL", params![key.carrier,key.call_key,task_id])?;
    Ok(())
}

/// Task observations settle only the exact restart-recovered execution;
/// live executions record their actual bounded terminal frame at the edge.
pub fn observe_task(
    conn: &Connection,
    carrier: &str,
    call_key: &str,
    task_id: &str,
    result: Option<&RecordedFrame>,
    outcome: &str,
) -> rusqlite::Result<bool> {
    let key = Key {
        carrier: carrier.into(),
        call_key: call_key.into(),
    };
    let recovered: bool = conn
        .query_row(
            "SELECT recovered=1 AND state='DispatchStarted' AND task_id=?3 FROM call_ledger WHERE carrier=?1 AND call_key=?2",
            params![carrier, call_key, task_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if let Some(frame) = result.filter(|_| recovered) {
        settle(conn, &key, frame, outcome, None, now_ms())?;
    }
    Ok(recovered)
}

#[cfg(feature = "test-timing-hooks")]
fn crash_point(state: State) {
    if !matches!(state, State::Prepared | State::Authorized) {
        return;
    }
    let Some(path) = std::env::var_os("AFT_TEST_CALL_LEDGER_CONTROL") else {
        return;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };
    if value["point"].as_str() != Some(state.as_str()) {
        return;
    }
    let Some(signal) = value["signal_path"].as_str() else {
        return;
    };
    if std::fs::write(signal, state.as_str()).is_ok() {
        // The harness kills this process after observing the committed state.
        loop {
            std::thread::park();
        }
    }
}
#[cfg(not(feature = "test-timing-hooks"))]
fn crash_point(_: State) {}

fn new_event_id() -> String {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("operating system random source for call event ids");
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        conn
    }
    fn key(carrier: &str) -> Key {
        Key {
            carrier: carrier.into(),
            call_key: "key".into(),
        }
    }
    fn scope(owner: &str) -> ScopeIdentity {
        ScopeIdentity {
            owner: owner.into(),
            scope_ref: "same-ref".into(),
            scope_epoch: 7,
        }
    }
    fn success() -> RecordedFrame {
        RecordedFrame {
            ty: FrameType::Response,
            body: br#"{ "isError":false,"content":[{"type":"text","text":"kept"}] }"#.to_vec(),
        }
    }

    #[test]
    fn call_ledger_admission_repeats_each_state_and_rejects_content_and_scope_conflicts() {
        let conn = database();
        let key = key("reserved:carrier");
        let scope = scope("reserved:owner");
        assert!(matches!(
            admit(&conn, &key, "digest", Some(&scope), State::Prepared).unwrap(),
            Admission::New
        ));
        for state in [State::Prepared, State::Authorized, State::DispatchStarted] {
            assert!(
                matches!(admit(&conn,&key,"digest",Some(&scope),state).unwrap(),Admission::Repeat(row) if row.state == state)
            );
            assert!(matches!(
                admit(&conn, &key, "different", Some(&scope), state).unwrap(),
                Admission::Conflict
            ));
            for other in [
                None,
                Some(self::scope("other-owner")),
                Some(ScopeIdentity {
                    scope_ref: "other-ref".into(),
                    ..scope.clone()
                }),
                Some(ScopeIdentity {
                    scope_epoch: 8,
                    ..scope.clone()
                }),
            ] {
                assert!(matches!(
                    admit(&conn, &key, "digest", other.as_ref(), state).unwrap(),
                    Admission::Conflict
                ));
            }
            if state == State::Prepared {
                assert!(transition(&conn, &key, state, State::Authorized).unwrap());
            }
            if state == State::Authorized {
                assert!(transition(&conn, &key, state, State::DispatchStarted).unwrap());
            }
        }
        let frame = success();
        assert!(settle(&conn, &key, &frame, "ok", None, 100).unwrap());
        assert!(!settle(
            &conn,
            &key,
            &not_retained(&key, "unknown"),
            "unknown",
            None,
            200
        )
        .unwrap());
        assert!(!transition(&conn, &key, State::DispatchStarted, State::Authorized).unwrap());
        match admit(&conn, &key, "digest", Some(&scope), State::Prepared).unwrap() {
            Admission::Repeat(row) => {
                assert_eq!(row.state, State::Settled);
                assert_eq!(row.frame.unwrap().body, frame.body);
                assert_eq!(row.seq, Some(1));
            }
            _ => panic!("expected settled replay"),
        }
    }

    #[test]
    fn call_ledger_restart_never_dispatches_prepared_or_authorized_and_unknown_is_not_completed() {
        let conn = database();
        let scope = scope("owner");
        for (carrier, state) in [
            ("prepared", State::Prepared),
            ("authorized", State::Authorized),
            ("read", State::DispatchStarted),
        ] {
            admit(&conn, &key(carrier), "digest", Some(&scope), state).unwrap();
        }
        recover(&conn).unwrap();
        recover(&conn).unwrap();
        for carrier in ["prepared", "authorized"] {
            let row = get(&conn, &key(carrier)).unwrap().unwrap();
            assert_eq!(row.state, State::Settled);
            assert_eq!(
                row.frame.unwrap().body,
                not_started(&key(carrier), "restart_before_dispatch").body
            );
            let entry: Value = serde_json::from_str(row.late_entry.as_deref().unwrap()).unwrap();
            assert_eq!(entry["kind"], "not_started");
            assert_eq!(entry["reason"], "restart_before_dispatch");
        }
        let row = get(&conn, &key("read")).unwrap().unwrap();
        assert_eq!(row.outcome.as_deref(), Some("unknown"));
        let entry: Value = serde_json::from_str(row.late_entry.as_deref().unwrap()).unwrap();
        assert_eq!(entry["kind"], "result");
        assert_eq!(entry["outcome"], "unknown");
        assert_eq!(entry["reduced"], true);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM call_ledger WHERE seq IS NOT NULL",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            3
        );
    }

    #[test]
    fn call_ledger_restart_does_not_attach_a_reused_key_to_an_old_task() {
        let conn = database();
        let key = key("reserved:carrier");
        conn.execute("INSERT INTO bash_tasks(harness,session_id,task_id,project_key,command,cwd,status,started_at,metadata) VALUES('h','s','old-task','p','command','cwd','completed',1,?1)", [json!({"call_key":{"requester":key.carrier,"key":key.call_key}}).to_string()]).unwrap();
        admit(
            &conn,
            &key,
            "new-read",
            Some(&scope("owner")),
            State::DispatchStarted,
        )
        .unwrap();
        recover(&conn).unwrap();
        let row = get(&conn, &key).unwrap().unwrap();
        assert_eq!(row.state, State::Settled);
        assert!(row.task_id.is_none());
        assert_eq!(row.outcome.as_deref(), Some("unknown"));
    }

    #[test]
    fn call_ledger_recovered_shell_settles_one_result_and_never_restarts() {
        let conn = database();
        let key = key("reserved:carrier");
        let scope = scope("owner");
        admit(&conn, &key, "digest", Some(&scope), State::DispatchStarted).unwrap();
        conn.execute("INSERT INTO bash_tasks(harness,session_id,task_id,project_key,command,cwd,status,started_at,metadata) VALUES('h','s','task','p','command','cwd','running',1,?1)",[json!({"call_key":{"requester":key.carrier,"key":key.call_key}}).to_string()]).unwrap();
        link_task(&conn, &key, "task").unwrap();
        recover(&conn).unwrap();
        assert_eq!(
            get(&conn, &key).unwrap().unwrap().state,
            State::DispatchStarted
        );
        let result = json!({"isError":false,"content":[]});
        let frame = RecordedFrame {
            ty: FrameType::Response,
            body: serde_json::to_vec(&result).unwrap(),
        };
        observe_task(
            &conn,
            &key.carrier,
            &key.call_key,
            "task",
            Some(&frame),
            "ok",
        )
        .unwrap();
        observe_task(
            &conn,
            &key.carrier,
            &key.call_key,
            "task",
            Some(&frame),
            "ok",
        )
        .unwrap();
        let row = get(&conn, &key).unwrap().unwrap();
        assert_eq!(row.seq, Some(1));
        assert_eq!(
            row.frame.unwrap().body,
            serde_json::to_vec(&result).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<Value>(row.late_entry.as_deref().unwrap()).unwrap()["kind"],
            "result"
        );
    }

    #[test]
    fn call_ledger_expiry_preserves_outcome_identity_and_frozen_answer() {
        let conn = database();
        let key = key("carrier");
        let scope = scope("owner");
        admit(&conn, &key, "digest", Some(&scope), State::DispatchStarted).unwrap();
        settle(&conn, &key, &success(), "ok", None, 100).unwrap();
        conn.execute(
            "UPDATE call_ledger SET withdraw_answer=?1",
            [b"frozen".as_slice()],
        )
        .unwrap();
        let before = get(&conn, &key).unwrap().unwrap();
        reduce_expired(&conn, &key, 100 + RETENTION_MS - 1).unwrap();
        assert_eq!(
            get(&conn, &key).unwrap().unwrap().frame.unwrap().body,
            success().body
        );
        reduce_expired(&conn, &key, 100 + RETENTION_MS).unwrap();
        let row = get(&conn, &key).unwrap().unwrap();
        assert_eq!(row.frame.unwrap().body, not_retained(&key, "ok").body);
        assert_eq!(row.event_id, before.event_id);
        assert_eq!(row.withdraw_answer, Some(b"frozen".to_vec()));
        assert_eq!(
            serde_json::from_str::<Value>(row.late_entry.as_deref().unwrap()).unwrap()["kind"],
            "expired"
        );
    }

    #[test]
    fn call_ledger_owner_identity_unscoped_ack_drop_and_bounded_sweep() {
        let conn = database();
        for owner in ["owner-a", "owner-b"] {
            let key = key(owner);
            admit(
                &conn,
                &key,
                "digest",
                Some(&scope(owner)),
                State::DispatchStarted,
            )
            .unwrap();
            settle(&conn, &key, &success(), "ok", None, 100).unwrap();
            assert_eq!(
                get(&conn, &key).unwrap().unwrap().scope.unwrap().owner,
                owner
            );
        }
        assert_eq!(ack(&conn, "owner-a", 1).unwrap(), 1);
        assert!(get(&conn, &key("owner-a")).unwrap().is_none());
        assert!(drop_entry(&conn, &key("owner-b")).unwrap());
        for i in 0..130 {
            let key = key(&format!("plain-{i}"));
            admit(&conn, &key, "digest", None, State::DispatchStarted).unwrap();
            settle(&conn, &key, &success(), "ok", None, 100).unwrap();
            let row = get(&conn, &key).unwrap().unwrap();
            assert!(row.scope.is_none());
            assert!(row.late_entry.is_none());
        }
        assert_eq!(sweep(&conn, 100 + RETENTION_MS - 1).unwrap(), 0);
        assert_eq!(sweep(&conn, 100 + RETENTION_MS).unwrap(), 64);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM call_ledger", [], |r| r
                .get::<_, usize>(0))
                .unwrap(),
            66
        );
        assert_eq!(sweep(&conn, 100 + RETENTION_MS).unwrap(), 64);
        assert_eq!(sweep(&conn, 100 + RETENTION_MS).unwrap(), 2);
    }
}
