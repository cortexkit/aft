//! Durable admission and settlement for keyed tool-provider calls.
//!
//! All operations use the actor's existing connection. State changes and late
//! entries share a transaction: a crash cannot publish half a settlement.
use cortexkit_role_tool_provider::late_results::{check_since, Cursor};
use cortexkit_role_tool_provider::withdraw::{CompletedResult, StartedOutcome, WithdrawAnswer};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subc_protocol::{ErrorBody, FrameType};

pub const RETENTION_MS: i64 = 24 * 60 * 60 * 1000;
pub const SWEEP_BATCH: usize = 64;
/// The contract's retention bounds (CONTRACT.md section 7): at most this many
/// unacked results per custodian and this many expired entries per scope
/// owner. AFT's custodian is always the scope's owner, so both count the same
/// column. The oldest entries are dropped first.
pub const UNACKED_RESULTS_MAX: usize = 1_000;
pub const EXPIRED_ENTRIES_MAX: usize = 1_000;
/// Most entries one `late_results` page reads, whatever `limit` asks for, so a
/// pull never walks an unbounded log.
pub const LATE_RESULTS_PAGE_MAX: u32 = 256;
/// The `not_started` reason of a held call that `tool.withdraw` settled.
pub const WITHDRAWN_REASON: &str = "withdrawn";
/// One trust-independent result snapshot per retained call, under the reply-size cap.
pub const MAX_TOOL_RESULT_SOURCE_BYTES: usize =
    cortexkit_role_tool_provider::result::MAX_RESULT_BYTES;

pub(super) const RESULT_SOURCE_MIGRATION: &str = r#"
ALTER TABLE call_ledger ADD COLUMN tool_result_source BLOB
    CHECK(tool_result_source IS NULL OR
        (typeof(tool_result_source) = 'blob' AND length(tool_result_source) > 0
            AND length(tool_result_source) < 3145728));
"#;

pub(super) fn migrate_result_source(conn: &Connection) -> rusqlite::Result<()> {
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('call_ledger') WHERE name='tool_result_source')", [], |row| row.get(0),
    )?;
    if !present {
        conn.execute_batch(RESULT_SOURCE_MIGRATION)?;
    }
    Ok(())
}

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
    pub tool_result_source: Option<Vec<u8>>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub fn get(conn: &Connection, key: &Key) -> rusqlite::Result<Option<Row>> {
    get_impl(conn, key, false)
}

fn get_impl(conn: &Connection, key: &Key, include_source: bool) -> rusqlite::Result<Option<Row>> {
    let sql = if include_source {
        "SELECT digest,state,owner,scope_ref,scope_epoch,task_id,withdraw_answer,frame_type,frame_body,outcome,event_id,settled_at,seq,late_entry,tool_result_source FROM call_ledger WHERE carrier=?1 AND call_key=?2"
    } else {
        "SELECT digest,state,owner,scope_ref,scope_epoch,task_id,withdraw_answer,frame_type,frame_body,outcome,event_id,settled_at,seq,late_entry FROM call_ledger WHERE carrier=?1 AND call_key=?2"
    };
    conn.query_row(sql, params![key.carrier, key.call_key], |r| {
        let owner: Option<String> = r.get(2)?;
        let ty: Option<String> = r.get(7)?;
        let body: Option<Vec<u8>> = r.get(8)?;
        Ok(Row {
            key: key.clone(),
            digest: r.get(0)?,
            state: match r.get::<_, String>(1)?.as_str() {
                "Prepared" => State::Prepared,
                "Authorized" => State::Authorized,
                "DispatchStarted" => State::DispatchStarted,
                _ => State::Settled,
            },
            scope: owner
                .map(|owner| {
                    Ok::<_, rusqlite::Error>(ScopeIdentity {
                        owner,
                        scope_ref: r.get(3)?,
                        scope_epoch: r.get(4)?,
                    })
                })
                .transpose()?,
            task_id: r.get(5)?,
            withdraw_answer: r.get(6)?,
            frame: ty.zip(body).map(|(ty, body)| RecordedFrame {
                ty: match ty.as_str() {
                    "Response" => FrameType::Response,
                    "StreamEnd" => FrameType::StreamEnd,
                    _ => FrameType::Error,
                },
                body,
            }),
            outcome: r.get(9)?,
            event_id: r.get(10)?,
            settled_at: r.get(11)?,
            seq: r.get(12)?,
            late_entry: r.get(13)?,
            tool_result_source: if include_source { r.get(14)? } else { None },
        })
    })
    .optional()
}

/// Replay alone loads the extra source; custody and maintenance keep reading the
/// original terminal bytes without pulling image blobs into their scans.
pub fn get_for_replay(conn: &Connection, key: &Key) -> rusqlite::Result<Option<Row>> {
    get_impl(conn, key, true)
}

fn load_tool_result_source(conn: &Connection, key: &Key) -> rusqlite::Result<Option<Vec<u8>>> {
    conn.query_row(
        "SELECT tool_result_source FROM call_ledger WHERE carrier=?1 AND call_key=?2",
        params![key.carrier, key.call_key],
        |record| record.get(0),
    )
    .optional()
    .map(Option::flatten)
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
    admit_with_tool_result_source(conn, key, digest, scope, state, false)
}

pub fn admit_with_tool_result_source(
    conn: &Connection,
    key: &Key,
    digest: &str,
    scope: Option<&ScopeIdentity>,
    state: State,
    include_body: bool,
) -> rusqlite::Result<Admission> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    if let Some(mut row) = get(&tx, key)? {
        return Ok(if row.digest == digest && row.scope.as_ref() == scope {
            if include_body && row.state == State::Settled {
                row.tool_result_source = load_tool_result_source(&tx, key)?;
            }
            Admission::Repeat(row)
        } else {
            Admission::Conflict
        });
    }
    #[cfg(test)]
    run_after_ledger_read(&tx);
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
    settle_with_tool_result_source(conn, key, frame, outcome, reason, now, None)
}

pub fn settle_with_tool_result_source(
    conn: &Connection,
    key: &Key,
    frame: &RecordedFrame,
    outcome: &str,
    reason: Option<&str>,
    now: i64,
    source: Option<&[u8]>,
) -> rusqlite::Result<bool> {
    if source.is_some_and(|source| {
        source.is_empty()
            || source.len() >= MAX_TOOL_RESULT_SOURCE_BYTES
            || frame.ty != FrameType::Response
    }) {
        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "tool result source requires a Response and must be nonempty and under 3 MiB",
            ),
        )));
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let Some(row) = get(&tx, key)? else {
        return Ok(false);
    };
    if row.state == State::Settled {
        return Ok(false);
    }
    #[cfg(test)]
    run_after_ledger_read(&tx);
    settle_row(&tx, &row, frame, outcome, reason, now, source)?;
    tx.commit()?;
    Ok(true)
}

/// Atomically settle Prepared and Authorized calls as scope-ended.
/// A concurrent approval cannot advance a settled row to DispatchStarted.
pub fn end_scope(conn: &Connection, scope: &ScopeIdentity, now: i64) -> rusqlite::Result<Vec<Key>> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let keys = {
        let mut statement = tx.prepare("SELECT carrier,call_key FROM call_ledger WHERE owner=?1 AND scope_ref=?2 AND scope_epoch=?3 AND state IN ('Prepared','Authorized')")?;
        let rows = statement.query_map(
            params![scope.owner, scope.scope_ref, scope.scope_epoch],
            |row| {
                Ok(Key {
                    carrier: row.get(0)?,
                    call_key: row.get(1)?,
                })
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for key in &keys {
        if let Some(row) = get(&tx, key)? {
            settle_row(
                &tx,
                &row,
                &not_started(key, "scope_ended"),
                "not_started",
                Some("scope_ended"),
                now,
                None,
            )?;
        }
    }
    tx.commit()?;
    Ok(keys)
}

/// Settles an unsettled row inside the caller's transaction. A settlement
/// with a `reason` means the call never ran, so its `tool.withdraw` answer
/// follows from that reason alone (see [`never_ran_answer`]: `withdrawn`, or
/// `refused` for a denial, an expired ask or an ended scope). That answer is
/// stored now because the reason is lost once the row expires: expiry
/// rewrites the recorded frame and the late entry, and an unscoped row has no
/// late entry at all. An answer already stored by an earlier withdraw is kept.
fn settle_row(
    conn: &Connection,
    row: &Row,
    frame: &RecordedFrame,
    outcome: &str,
    reason: Option<&str>,
    now: i64,
    tool_result_source: Option<&[u8]>,
) -> rusqlite::Result<()> {
    let key = &row.key;
    let tx = conn;
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
    let never_ran_answer = reason.map(|reason| answer_bytes(&never_ran_answer(reason)));
    tx.execute("UPDATE call_ledger SET state='Settled',frame_type=?3,frame_body=?4,outcome=?5,settled_at=?6,seq=?7,late_entry=?8,withdraw_answer=COALESCE(withdraw_answer,?10),tool_result_source=?11 WHERE carrier=?1 AND call_key=?2 AND state=?9",
        params![key.carrier,key.call_key,frame.type_name(),frame.body,outcome,now,seq,entry,row.state.as_str(),never_ran_answer,tool_result_source])?;
    if let Some(scope) = &row.scope {
        enforce_retention_bounds(tx, &scope.owner)?;
    }
    Ok(())
}

/// The withdraw answer of a call that never ran, from its not-started reason.
fn never_ran_answer(reason: &str) -> WithdrawAnswer {
    use cortexkit_role_tool_provider::withdraw::{reasons, RefusalReason};
    match reason {
        reasons::DENIED => WithdrawAnswer::Refused {
            reason: RefusalReason::Denied,
        },
        reasons::EXPIRED => WithdrawAnswer::Refused {
            reason: RefusalReason::Expired,
        },
        reasons::SCOPE_ENDED => WithdrawAnswer::Refused {
            reason: RefusalReason::ScopeEnded,
        },
        _ => WithdrawAnswer::Withdrawn,
    }
}

fn answer_bytes(answer: &WithdrawAnswer) -> Vec<u8> {
    serde_json::to_vec(&answer.encode()).expect("withdraw answers are JSON")
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
    let entry = row
        .late_entry
        .map(|entry| -> rusqlite::Result<String> {
            let corrupt = |error: Box<dyn std::error::Error + Send + Sync>| {
                rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, error)
            };
            let mut value: Value =
                serde_json::from_str(&entry).map_err(|error| corrupt(Box::new(error)))?;
            let object = value.as_object_mut().ok_or_else(|| {
                corrupt(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "call ledger late entry is not an object",
                )))
            })?;
            object.insert("kind".into(), json!("expired"));
            object.insert("reduced".into(), json!(true));
            object.remove("result");
            object.remove("reason");
            Ok(value.to_string())
        })
        .transpose()?;
    conn.execute("UPDATE call_ledger SET frame_type='Error',frame_body=?3,late_entry=?4,tool_result_source=NULL WHERE carrier=?1 AND call_key=?2", params![key.carrier,key.call_key,frame.body,entry])?;
    Ok(())
}

/// A SQL LIMIT bounds the work before collecting any rows.
pub fn sweep(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let mut stmt = tx.prepare("SELECT carrier,call_key FROM call_ledger WHERE state='Settled' AND (acked=1 OR (settled_at<=?1 AND (owner IS NULL OR json_extract(late_entry,'$.kind')!='expired'))) ORDER BY settled_at,carrier,call_key LIMIT ?2")?;
    let keys = stmt
        .query_map(params![now - RETENTION_MS, SWEEP_BATCH], |r| {
            Ok(Key {
                carrier: r.get(0)?,
                call_key: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let mut delete = tx.prepare(
        "DELETE FROM call_ledger WHERE carrier=?1 AND call_key=?2 AND (acked=1 OR owner IS NULL)",
    )?;
    for key in &keys {
        delete.execute(params![key.carrier, key.call_key])?;
        reduce_expired(&tx, key, now)?;
    }
    drop(delete);
    tx.commit()?;
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

/// Drops one custodian's oldest late entries beyond [`UNACKED_RESULTS_MAX`]
/// unacked results and [`EXPIRED_ENTRIES_MAX`] expired entries (the
/// tool-provider contract's retention bounds), deleting their rows as an ack
/// would.
fn enforce_retention_bounds(conn: &Connection, custodian: &str) -> rusqlite::Result<()> {
    for (expired, max) in [(false, UNACKED_RESULTS_MAX), (true, EXPIRED_ENTRIES_MAX)] {
        conn.execute(
            "DELETE FROM call_ledger WHERE rowid IN (SELECT rowid FROM call_ledger
                WHERE custodian=?1 AND late_entry IS NOT NULL
                AND (json_extract(late_entry,'$.kind')='expired')=?2
                ORDER BY seq DESC LIMIT -1 OFFSET ?3)",
            params![custodian, expired, max as i64],
        )?;
    }
    Ok(())
}

fn parse_entry(entry: &str) -> rusqlite::Result<Value> {
    let corrupt = |error: Box<dyn std::error::Error + Send + Sync>| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, error)
    };
    let value: Value = serde_json::from_str(entry).map_err(|error| corrupt(Box::new(error)))?;
    if !value.is_object() {
        return Err(corrupt(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "call ledger late entry is not an object",
        ))));
    }
    Ok(value)
}

/// Keeps only the outcome of a settled row whose result is too large for one
/// reply frame (the `max_body` bound of [`withdraw`] and [`late_results`]),
/// so later replays, withdraw answers and late entries all agree that the
/// result is gone. Returns the kept outcome.
fn reduce_payload(conn: &Connection, row: &Row) -> rusqlite::Result<String> {
    let outcome = row.outcome.clone().unwrap_or_else(|| "unknown".into());
    let frame = not_retained(&row.key, &outcome);
    let entry = row
        .late_entry
        .as_deref()
        .map(|entry| -> rusqlite::Result<String> {
            let mut value = parse_entry(entry)?;
            let object = value.as_object_mut().expect("parse_entry returns objects");
            if object.remove("result").is_some() {
                object.insert("reduced".into(), json!(true));
                object.insert("outcome".into(), json!(outcome));
            }
            Ok(value.to_string())
        })
        .transpose()?;
    conn.execute(
        "UPDATE call_ledger SET frame_type='Error',frame_body=?3,late_entry=?4,tool_result_source=NULL WHERE carrier=?1 AND call_key=?2",
        params![row.key.carrier, row.key.call_key, frame.body, entry],
    )?;
    Ok(outcome)
}

/// What a `tool.withdraw` found under one `(carrier, call_key)`.
#[derive(Debug, PartialEq, Eq)]
pub enum Withdrawal {
    /// No record has that key.
    UnknownCall,
    /// The record is under another scope than the caller's route stamp.
    NotPermitted,
    /// The answer frozen on the record, as reply body bytes.
    Answer {
        body: Vec<u8>,
        /// The shell task to kill. Set while the call is running with a
        /// linked task: `body` is then NOT stored yet. The caller stores it
        /// with [`freeze_withdraw_answer`] once the kill has landed, so a
        /// withdraw whose kill failed leaves no answer behind and a retry
        /// tries the kill again.
        kill_task: Option<String>,
        /// This withdraw settled a held call that never dispatched, so its
        /// held request must now end with `not_started`.
        held_withdrawn: bool,
    },
}

/// Answers `tool.withdraw` from the record, in one conditional transaction.
///
/// The first withdraw computes the answer from the settlement table and
/// stores its bytes; every later withdraw by any permitted caller returns
/// those bytes, whatever expiry later does to the row. The one exception is a
/// running call with a linked shell task: its answer is returned with the task
/// to kill and stored only by [`freeze_withdraw_answer`] after the kill. A
/// held call that has not dispatched is settled `not_started` with reason
/// `withdrawn` here, so the admission step that would dispatch it finds the
/// row settled and stops.
/// `max_body` bounds the answer: a `completed` answer that would not fit is
/// reduced to its outcome, and the row with it.
pub fn withdraw(
    conn: &Connection,
    key: &Key,
    stamp: Option<&ScopeIdentity>,
    now: i64,
    max_body: usize,
) -> rusqlite::Result<Withdrawal> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    reduce_expired(&tx, key, now)?;
    // Expiry is committed even when nothing else changes, as at admission.
    let Some(row) = get(&tx, key)? else {
        tx.commit()?;
        return Ok(Withdrawal::UnknownCall);
    };
    if row.scope.as_ref() != stamp {
        tx.commit()?;
        return Ok(Withdrawal::NotPermitted);
    }
    if let Some(body) = row.withdraw_answer {
        tx.commit()?;
        return Ok(Withdrawal::Answer {
            body,
            kill_task: None,
            held_withdrawn: false,
        });
    }
    let mut kill_task = None;
    let mut held_withdrawn = false;
    let answer = match row.state {
        State::Prepared | State::Authorized => {
            settle_row(
                &tx,
                &row,
                &not_started(key, WITHDRAWN_REASON),
                "not_started",
                Some(WITHDRAWN_REASON),
                now,
                None,
            )?;
            held_withdrawn = true;
            WithdrawAnswer::Withdrawn
        }
        State::DispatchStarted => {
            kill_task = row.task_id.clone();
            WithdrawAnswer::AlreadyStarted {
                outcome: StartedOutcome::Unknown,
            }
        }
        State::Settled => {
            let answer = settled_answer(&row);
            if answer_bytes(&answer).len() > max_body {
                WithdrawAnswer::Completed {
                    result: CompletedResult::OutcomeOnly(reduce_payload(&tx, &row)?),
                }
            } else {
                answer
            }
        }
    };
    let body = answer_bytes(&answer);
    if kill_task.is_none() {
        tx.execute(
            "UPDATE call_ledger SET withdraw_answer=?3 WHERE carrier=?1 AND call_key=?2",
            params![key.carrier, key.call_key, body],
        )?;
    }
    tx.commit()?;
    Ok(Withdrawal::Answer {
        body,
        kill_task,
        held_withdrawn,
    })
}

/// Stores the answer of a withdraw whose kill landed, unless another withdraw
/// stored one first, and returns the stored answer. A row that is gone (acked
/// or dropped meanwhile) keeps nothing, and `body` is returned as is.
pub fn freeze_withdraw_answer(
    conn: &Connection,
    key: &Key,
    body: &[u8],
) -> rusqlite::Result<Vec<u8>> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE call_ledger SET withdraw_answer=COALESCE(withdraw_answer,?3) WHERE carrier=?1 AND call_key=?2",
        params![key.carrier, key.call_key, body],
    )?;
    let stored = get(&tx, key)?
        .and_then(|row| row.withdraw_answer)
        .unwrap_or_else(|| body.to_vec());
    tx.commit()?;
    Ok(stored)
}

/// The `tool.withdraw` answer for a row that has already settled and has no
/// stored answer yet. `completed` is answered only when AFT recorded the
/// call's terminal frame; a row whose end was never observed is
/// `already_started` with outcome `unknown`.
fn settled_answer(row: &Row) -> WithdrawAnswer {
    let outcome = row.outcome.as_deref().unwrap_or("unknown");
    let Some(frame) = &row.frame else {
        return WithdrawAnswer::AlreadyStarted {
            outcome: StartedOutcome::Unknown,
        };
    };
    let body: Value = serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
    if frame.ty == FrameType::Error {
        match body["code"].as_str() {
            Some("not_started") => {
                return never_ran_answer(body["detail"]["reason"].as_str().unwrap_or(""))
            }
            Some("result_not_retained") => {
                return match outcome {
                    "unknown" => WithdrawAnswer::AlreadyStarted {
                        outcome: StartedOutcome::Unknown,
                    },
                    "not_started" => WithdrawAnswer::Withdrawn,
                    outcome => WithdrawAnswer::Completed {
                        result: CompletedResult::OutcomeOnly(outcome.into()),
                    },
                }
            }
            _ => {}
        }
    }
    WithdrawAnswer::Completed {
        result: CompletedResult::Retained(body),
    }
}

fn head_seq(conn: &Connection, custodian: &str) -> rusqlite::Result<u64> {
    Ok(conn
        .query_row(
            "SELECT last_seq FROM call_ledger_ack WHERE custodian=?1",
            [custodian],
            |r| r.get::<_, u64>(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Serves one `late_results` page to `custodian`, as reply body bytes, or the
/// cursor refusal.
///
/// Entries come oldest first after `since`. The page stops before the entry
/// that would push the encoded reply past `max_body` and says `more: true`;
/// an entry that would overflow `max_body` on its own is reduced to its
/// outcome, and its row with it, before it is served. An entry past the
/// retention period is turned into an `expired` entry in place first, and the
/// retention bounds ([`UNACKED_RESULTS_MAX`], [`EXPIRED_ENTRIES_MAX`]) are
/// applied before reading. At most [`LATE_RESULTS_PAGE_MAX`] rows are read.
pub fn late_results(
    conn: &Connection,
    custodian: &str,
    incarnation: &str,
    since: Option<&Cursor>,
    limit: Option<u32>,
    now: i64,
    max_body: usize,
) -> rusqlite::Result<Result<Vec<u8>, ErrorBody>> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let head = head_seq(&tx, custodian)?;
    if let Err(error) = check_since(incarnation, head, since) {
        return Ok(Err(error));
    }
    enforce_retention_bounds(&tx, custodian)?;
    let since_seq = since.map_or(0, |cursor| cursor.seq);
    let cap = limit
        .unwrap_or(LATE_RESULTS_PAGE_MAX)
        .min(LATE_RESULTS_PAGE_MAX) as usize;
    let rows = {
        let mut stmt = tx.prepare(
            "SELECT carrier,call_key,seq FROM call_ledger
             WHERE custodian=?1 AND seq>?2 AND late_entry IS NOT NULL ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![custodian, since_seq, cap as i64 + 1], |r| {
                Ok((
                    Key {
                        carrier: r.get(0)?,
                        call_key: r.get(1)?,
                    },
                    r.get::<_, u64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    // The largest wrapper a page can have: the longest seq and `false`.
    let overhead = serde_json::to_vec(&json!({
        "entries": [],
        "cursor": {"provider_incarnation": incarnation, "seq": u64::MAX},
        "more": false,
    }))
    .expect("late results reply is JSON")
    .len();
    let mut more = rows.len() > cap;
    let mut size = overhead;
    let mut last = since_seq;
    let mut entries = Vec::new();
    for (key, seq) in rows.into_iter().take(cap) {
        let Some(mut row) = get(&tx, &key)? else {
            continue;
        };
        let due = row.settled_at.is_some_and(|at| at + RETENTION_MS <= now);
        let entry_kind = |row: &Row| -> rusqlite::Result<Option<String>> {
            row.late_entry
                .as_deref()
                .map(|entry| Ok(parse_entry(entry)?["kind"].as_str().map(str::to_owned)))
                .transpose()
                .map(Option::flatten)
        };
        if due && entry_kind(&row)?.as_deref() != Some("expired") {
            reduce_expired(&tx, &key, now)?;
            row = get(&tx, &key)?.expect("the row read above still exists");
        }
        let mut entry = parse_entry(row.late_entry.as_deref().unwrap_or("null"))?;
        let mut len = serde_json::to_vec(&entry).expect("entry is JSON").len();
        if overhead + len > max_body {
            reduce_payload(&tx, &row)?;
            row = get(&tx, &key)?.expect("the row read above still exists");
            entry = parse_entry(row.late_entry.as_deref().unwrap_or("null"))?;
            len = serde_json::to_vec(&entry).expect("entry is JSON").len();
        }
        let added = len + usize::from(!entries.is_empty());
        if size + added > max_body {
            more = true;
            break;
        }
        size += added;
        last = seq;
        entries.push(entry);
    }
    tx.commit()?;
    let reply = json!({
        "entries": entries,
        "cursor": {"provider_incarnation": incarnation, "seq": last},
        "more": more,
    });
    Ok(Ok(
        serde_json::to_vec(&reply).expect("late results reply is JSON")
    ))
}

/// `late_results.ack {through}` for `custodian`: refuses a cursor from another
/// incarnation or past the log, otherwise deletes every entry through it.
pub fn ack_through(
    conn: &Connection,
    custodian: &str,
    incarnation: &str,
    through: &Cursor,
) -> rusqlite::Result<Result<usize, ErrorBody>> {
    let head = head_seq(conn, custodian)?;
    if let Err(error) = check_since(incarnation, head, Some(through)) {
        return Ok(Err(error));
    }
    ack(conn, custodian, through.seq).map(Ok)
}

/// Whether this process admits keyed tool-provider calls and therefore owns
/// restart recovery of the call ledger. Only the subc module admits ledger
/// rows; standalone sessions and CLI commands share the same aft.db but never
/// write a row, so they must not settle the module's in-flight calls. Unit
/// tests run as a provider so their direct ledger fixtures keep recovering.
static LEDGER_PROVIDER_PROCESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(cfg!(test));

/// Marks this process as the call ledger's provider. The subc module calls it
/// before it can open a database, so its first pool runs restart recovery.
pub fn mark_provider_process() {
    LEDGER_PROVIDER_PROCESS.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn is_provider_process() -> bool {
    LEDGER_PROVIDER_PROCESS.load(std::sync::atomic::Ordering::SeqCst)
}

/// Called once when a process installs its database, before admitting calls.
/// A dispatch with a durable task is observed, never re-sent.
///
/// Recovery settles every unsettled row in the file. Rows record no owning
/// process, so this is correct only while one module process owns a given
/// aft.db's call ledger: a second module sharing the file would have its
/// in-flight calls settled here as not_started or unknown. A change that lets
/// two modules share one storage root must first add an owner column and limit
/// recovery to rows whose owner is gone. Processes that never admit calls skip
/// recovery entirely (see [`recover_pool_once`]).
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
        #[cfg(test)]
        run_before_recovery_read(conn, &key);
        let Some(row) = get(conn, &key)? else {
            continue;
        };
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
///
/// Does nothing outside the provider process: a standalone session opening
/// the shared aft.db has no ledger rows of its own, and every unsettled row
/// there may belong to a module that is still running the call.
pub fn recover_pool_once(
    pool: &std::sync::Arc<std::sync::Mutex<super::TrackedConnection>>,
) -> rusqlite::Result<()> {
    if !is_provider_process() {
        return Ok(());
    }
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
    observe_task_with_tool_result_source(conn, carrier, call_key, task_id, result, outcome, None)
}

pub fn observe_task_with_tool_result_source(
    conn: &Connection,
    carrier: &str,
    call_key: &str,
    task_id: &str,
    result: Option<&RecordedFrame>,
    outcome: &str,
    body: Option<&[u8]>,
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
        settle_with_tool_result_source(conn, &key, frame, outcome, None, now_ms(), body)?;
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
type LedgerReadHook = Box<dyn FnOnce(&Connection)>;
#[cfg(test)]
type RecoveryReadHook = Box<dyn FnOnce(&Connection, &Key)>;
#[cfg(test)]
thread_local! {
    static AFTER_LEDGER_READ: std::cell::RefCell<Option<LedgerReadHook>> = const { std::cell::RefCell::new(None) };
    static BEFORE_RECOVERY_READ: std::cell::RefCell<Option<RecoveryReadHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_after_ledger_read(conn: &Connection) {
    let hook = AFTER_LEDGER_READ.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(conn);
    }
}

#[cfg(test)]
fn run_before_recovery_read(conn: &Connection, key: &Key) {
    let hook = BEFORE_RECOVERY_READ.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(conn, key);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scope_end_fences_only_matching_pre_dispatch_rows() {
        let conn = database();
        let own = scope("owner-a");
        for (carrier, state, stamp) in [
            ("prepared", State::Prepared, own.clone()),
            ("authorized", State::Authorized, own.clone()),
            ("started", State::DispatchStarted, own.clone()),
            ("other-owner", State::Prepared, scope("owner-b")),
            (
                "other-epoch",
                State::Prepared,
                ScopeIdentity {
                    scope_epoch: own.scope_epoch + 1,
                    ..own.clone()
                },
            ),
        ] {
            admit(&conn, &key(carrier), "content", Some(&stamp), state).unwrap();
        }
        let ended = end_scope(&conn, &own, now_ms()).unwrap();
        assert_eq!(ended.len(), 2);
        for carrier in ["prepared", "authorized"] {
            let row = get(&conn, &key(carrier)).unwrap().unwrap();
            assert_eq!(row.state, State::Settled);
            assert_eq!(
                serde_json::from_slice::<Value>(&row.withdraw_answer.unwrap()).unwrap(),
                json!({"answer":"refused","reason":"scope_ended"})
            );
            assert_eq!(
                serde_json::from_str::<Value>(row.late_entry.as_ref().unwrap()).unwrap()["reason"],
                "scope_ended"
            );
            assert!(!transition(
                &conn,
                &key(carrier),
                State::Authorized,
                State::DispatchStarted
            )
            .unwrap());
        }
        assert_eq!(
            get(&conn, &key("started")).unwrap().unwrap().state,
            State::DispatchStarted
        );
        for carrier in ["other-owner", "other-epoch"] {
            assert_eq!(
                get(&conn, &key(carrier)).unwrap().unwrap().state,
                State::Prepared
            );
        }
        assert!(end_scope(&conn, &own, now_ms()).unwrap().is_empty());
    }
    use super::*;

    #[test]
    fn concurrent_ledger_writers_do_not_upgrade_stale_snapshots() {
        const CHILD_DB: &str = "AFT_LEDGER_RACE_DB";
        const CHILD_OP: &str = "AFT_LEDGER_RACE_OPERATION";
        const CHILD_RETRY: &str = "AFT_LEDGER_RACE_RETRY";
        let competitor = |path: &std::path::Path, operation: &str, retry: bool| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["db::call_ledger::tests::concurrent_ledger_writers_do_not_upgrade_stale_snapshots", "--exact", "--nocapture"])
                .env(CHILD_DB, path).env(CHILD_OP, operation);
            if retry {
                command.env(CHILD_RETRY, "1");
            } else {
                command.env_remove(CHILD_RETRY);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "competitor: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        if let Some(path) = std::env::var_os(CHILD_DB) {
            let conn = Connection::open(path).unwrap();
            conn.busy_timeout(std::time::Duration::ZERO).unwrap();
            let retry = std::env::var_os(CHILD_RETRY).is_some();
            let result = if std::env::var(CHILD_OP).unwrap() == "admit" {
                admit(&conn, &key("race"), "digest", None, State::DispatchStarted).map(
                    |admission| {
                        if retry {
                            matches!(admission, Admission::Repeat(_))
                        } else {
                            !matches!(admission, Admission::Conflict)
                        }
                    },
                )
            } else {
                settle(&conn, &key("race"), &success(), "ok", None, 100)
                    .map(|changed| !retry || !changed)
            };
            match result {
                Ok(expected) => {
                    assert!(expected, "competitor did not repeat or settle idempotently")
                }
                Err(error) => assert!(
                    !retry
                        && matches!(error, rusqlite::Error::SqliteFailure(ref code, _) if code.code == rusqlite::ErrorCode::DatabaseBusy),
                    "unexpected competitor error: {error}"
                ),
            }
            return;
        }
        let mut failures = Vec::new();
        for operation in ["admit", "settle"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ledger.sqlite");
            let mut conn = Connection::open(&path).unwrap();
            crate::db::run_migrations(&mut conn).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL").unwrap();
            if operation == "settle" {
                admit(&conn, &key("race"), "digest", None, State::DispatchStarted).unwrap();
            }
            let competed = std::rc::Rc::new(std::cell::Cell::new(false));
            let hook_competed = competed.clone();
            let hook_path = path.clone();
            AFTER_LEDGER_READ.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move |_| {
                    hook_competed.set(true);
                    competitor(&hook_path, operation, false);
                }))
            });
            if operation == "admit" {
                let result = admit(&conn, &key("race"), "digest", None, State::DispatchStarted);
                assert!(competed.get(), "admission competitor did not run");
                if let Err(error) = &result {
                    failures.push(format!("admit: {error}"));
                    continue;
                }
                assert!(matches!(result.unwrap(), Admission::New));
                assert!(matches!(
                    admit(&conn, &key("race"), "digest", None, State::DispatchStarted).unwrap(),
                    Admission::Repeat(_)
                ));
            } else {
                let result = settle(&conn, &key("race"), &success(), "ok", None, 100);
                assert!(competed.get(), "settlement competitor did not run");
                if let Err(error) = &result {
                    failures.push(format!("settle: {error}"));
                    continue;
                }
                assert!(result.unwrap());
                assert!(!settle(&conn, &key("race"), &success(), "ok", None, 100).unwrap());
            }
            competitor(&path, operation, true);
        }
        assert!(
            failures.is_empty(),
            "stale transaction upgrades: {failures:?}"
        );
    }

    #[test]
    fn corrupt_expired_ledger_entry_returns_error_without_mutation() {
        for damaged in ["{", "[]", "null"] {
            let conn = database();
            let key = key("corrupt");
            admit(
                &conn,
                &key,
                "digest",
                Some(&scope("owner")),
                State::DispatchStarted,
            )
            .unwrap();
            settle(&conn, &key, &success(), "ok", None, 100).unwrap();
            conn.execute("UPDATE call_ledger SET late_entry=?1", [damaged])
                .unwrap();
            assert!(reduce_expired(&conn, &key, 100 + RETENTION_MS).is_err());
            let row = get(&conn, &key).unwrap().unwrap();
            assert_eq!(row.late_entry.as_deref(), Some(damaged));
            assert_eq!(row.frame.unwrap().body, success().body);
        }
    }

    #[test]
    fn ledger_recovery_tolerates_concurrent_row_removal() {
        let conn = database();
        let key = key("disappearing");
        admit(&conn, &key, "digest", None, State::Prepared).unwrap();
        BEFORE_RECOVERY_READ.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|conn, key| {
                conn.execute(
                    "DELETE FROM call_ledger WHERE carrier=?1 AND call_key=?2",
                    params![key.carrier, key.call_key],
                )
                .unwrap();
            }))
        });
        recover(&conn).unwrap();
        assert!(get(&conn, &key).unwrap().is_none());
    }

    #[test]
    fn ledger_sweep_rolls_back_earlier_deletes_on_failure() {
        let conn = database();
        for carrier in ["a", "b"] {
            admit(&conn, &key(carrier), "digest", None, State::DispatchStarted).unwrap();
            settle(&conn, &key(carrier), &success(), "ok", None, 100).unwrap();
        }
        conn.execute_batch("CREATE TRIGGER fail_second_delete BEFORE DELETE ON call_ledger WHEN OLD.carrier='b' BEGIN SELECT RAISE(ABORT, 'injected delete failure'); END").unwrap();
        assert!(sweep(&conn, 100 + RETENTION_MS).is_err());
        assert!(
            get(&conn, &key("a")).unwrap().is_some(),
            "earlier batch deletion was not rolled back"
        );
        assert!(get(&conn, &key("b")).unwrap().is_some());
    }
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
    fn tool_result_source_is_bounded_atomic_and_cleared_with_payload() {
        let conn = database();
        let key = key("retained-body");
        let scope = scope("owner");
        assert!(matches!(
            admit(&conn, &key, "digest", Some(&scope), State::DispatchStarted).unwrap(),
            Admission::New
        ));
        let legacy = success();
        let oversized = vec![b'x'; MAX_TOOL_RESULT_SOURCE_BYTES];
        assert!(settle_with_tool_result_source(
            &conn,
            &key,
            &legacy,
            "ok",
            None,
            1,
            Some(&oversized)
        )
        .is_err());
        assert!(conn
            .execute("UPDATE call_ledger SET tool_result_source=?1", [&oversized])
            .is_err());
        assert_eq!(
            get(&conn, &key).unwrap().unwrap().state,
            State::DispatchStarted
        );
        let body = br#"{"content":[{"type":"text","text":"private-projection"}],"isError":false}"#;
        conn.execute_batch("CREATE TRIGGER fail_result_body BEFORE UPDATE OF tool_result_source ON call_ledger WHEN NEW.tool_result_source IS NOT NULL BEGIN SELECT RAISE(ABORT,'injected result body failure'); END;").unwrap();
        assert!(
            settle_with_tool_result_source(&conn, &key, &legacy, "ok", None, 1, Some(body))
                .is_err()
        );
        let row = get_for_replay(&conn, &key).unwrap().unwrap();
        assert_eq!(row.state, State::DispatchStarted);
        assert!(
            row.frame.is_none() && row.tool_result_source.is_none() && row.late_entry.is_none()
        );
        conn.execute_batch("DROP TRIGGER fail_result_body").unwrap();
        assert!(
            settle_with_tool_result_source(&conn, &key, &legacy, "ok", None, 1, Some(body))
                .unwrap()
        );
        let row = get_for_replay(&conn, &key).unwrap().unwrap();
        assert_eq!(row.frame.as_ref().unwrap().body, legacy.body);
        assert_eq!(row.tool_result_source.as_deref(), Some(body.as_slice()));
        assert!(
            get(&conn, &key)
                .unwrap()
                .unwrap()
                .tool_result_source
                .is_none(),
            "ordinary scans do not load cached images"
        );
        assert_eq!(
            serde_json::from_str::<Value>(row.late_entry.as_ref().unwrap()).unwrap()["result"],
            serde_json::from_slice::<Value>(&legacy.body).unwrap()
        );
        assert!(!settled_answer(&row)
            .encode()
            .to_string()
            .contains("private-projection"));
        reduce_expired(&conn, &key, RETENTION_MS + 2).unwrap();
        assert!(get_for_replay(&conn, &key)
            .unwrap()
            .unwrap()
            .tool_result_source
            .is_none());

        let reduced = Key {
            carrier: "retained-body".into(),
            call_key: "reduced".into(),
        };
        admit(
            &conn,
            &reduced,
            "digest",
            Some(&scope),
            State::DispatchStarted,
        )
        .unwrap();
        settle_with_tool_result_source(&conn, &reduced, &legacy, "ok", None, 1, Some(body))
            .unwrap();
        reduce_payload(&conn, &get(&conn, &reduced).unwrap().unwrap()).unwrap();
        assert!(get_for_replay(&conn, &reduced)
            .unwrap()
            .unwrap()
            .tool_result_source
            .is_none());
        let near_limit = Key {
            carrier: "retained-body".into(),
            call_key: "near-limit".into(),
        };
        admit(&conn, &near_limit, "digest", None, State::DispatchStarted).unwrap();
        let mut large_body = body.to_vec();
        large_body.resize(MAX_TOOL_RESULT_SOURCE_BYTES - 1, b' ');
        assert!(settle_with_tool_result_source(
            &conn,
            &near_limit,
            &legacy,
            "ok",
            None,
            1,
            Some(&large_body)
        )
        .unwrap());
        assert_eq!(
            get_for_replay(&conn, &near_limit)
                .unwrap()
                .unwrap()
                .tool_result_source
                .unwrap()
                .len(),
            MAX_TOOL_RESULT_SOURCE_BYTES - 1
        );
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

    fn answer(found: Withdrawal) -> (Value, Option<String>, bool) {
        match found {
            Withdrawal::Answer {
                body,
                kill_task,
                held_withdrawn,
            } => (
                serde_json::from_slice(&body).unwrap(),
                kill_task,
                held_withdrawn,
            ),
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    fn withdraw_bytes(
        conn: &Connection,
        key: &Key,
        stamp: Option<&ScopeIdentity>,
        now: i64,
    ) -> Vec<u8> {
        match withdraw(conn, key, stamp, now, usize::MAX).unwrap() {
            Withdrawal::Answer { body, .. } => body,
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    #[test]
    fn withdraw_answers_follow_the_settlement_table_and_freeze_on_first_withdraw() {
        let conn = database();
        let scope = scope("owner");
        let stamp = Some(&scope);
        let held = Key {
            carrier: "carrier".into(),
            call_key: "held".into(),
        };
        admit(&conn, &held, "digest", stamp, State::Prepared).unwrap();
        let (first, kill, held_withdrawn) =
            answer(withdraw(&conn, &held, stamp, 100, usize::MAX).unwrap());
        assert_eq!(first, json!({"answer": "withdrawn"}));
        assert_eq!((kill, held_withdrawn), (None, true));
        let row = get(&conn, &held).unwrap().unwrap();
        assert_eq!(row.state, State::Settled);
        assert_eq!(
            row.frame.unwrap().body,
            not_started(&held, WITHDRAWN_REASON).body
        );
        let entry: Value = serde_json::from_str(row.late_entry.as_deref().unwrap()).unwrap();
        assert_eq!(
            (entry["kind"].as_str(), entry["reason"].as_str()),
            (Some("not_started"), Some("withdrawn"))
        );
        // A settled row can no longer be authorized or dispatched.
        assert!(!transition(&conn, &held, State::Prepared, State::Authorized).unwrap());
        assert_eq!(
            answer(withdraw(&conn, &held, stamp, 200, usize::MAX).unwrap()),
            (first, None, false)
        );

        // A running shell call: each withdraw names its task to kill until
        // one stores the answer after its kill landed; a withdraw whose kill
        // failed stores nothing, so the next one tries the kill again. After
        // the freeze, later ones return the frozen bytes and kill nothing.
        // Settlement after the freeze does not change the answer.
        let running = Key {
            carrier: "carrier".into(),
            call_key: "running".into(),
        };
        admit(&conn, &running, "digest", stamp, State::DispatchStarted).unwrap();
        link_task(&conn, &running, "task-1").unwrap();
        let (first, kill, held_withdrawn) =
            answer(withdraw(&conn, &running, stamp, 100, usize::MAX).unwrap());
        assert_eq!(
            first,
            json!({"answer": "already_started", "outcome": "unknown"})
        );
        assert_eq!((kill.as_deref(), held_withdrawn), (Some("task-1"), false));
        assert!(get(&conn, &running)
            .unwrap()
            .unwrap()
            .withdraw_answer
            .is_none());
        let (retried, kill, _) = answer(withdraw(&conn, &running, stamp, 100, usize::MAX).unwrap());
        assert_eq!((&retried, kill.as_deref()), (&first, Some("task-1")));
        let first_bytes = withdraw_bytes(&conn, &running, stamp, 100);
        assert_eq!(
            freeze_withdraw_answer(&conn, &running, &first_bytes).unwrap(),
            first_bytes
        );
        // A second kill that lands later keeps the answer stored first.
        assert_eq!(
            freeze_withdraw_answer(&conn, &running, b"later").unwrap(),
            first_bytes
        );
        assert_eq!(
            answer(withdraw(&conn, &running, stamp, 100, usize::MAX).unwrap()),
            (first.clone(), None, false)
        );
        settle(&conn, &running, &success(), "ok", None, 150).unwrap();
        assert_eq!(
            answer(withdraw(&conn, &running, stamp, 200, usize::MAX).unwrap()),
            (first, None, false)
        );

        // A non-shell call at DispatchStarted has no task: nothing is killed.
        let read = Key {
            carrier: "carrier".into(),
            call_key: "read".into(),
        };
        admit(&conn, &read, "digest", stamp, State::DispatchStarted).unwrap();
        assert_eq!(
            answer(withdraw(&conn, &read, stamp, 100, usize::MAX).unwrap()).1,
            None
        );

        // Never-run settlements.
        for (call_key, reason, expected) in [
            (
                "denied",
                "denied",
                json!({"answer": "refused", "reason": "denied"}),
            ),
            (
                "ask-expired",
                "expired",
                json!({"answer": "refused", "reason": "expired"}),
            ),
            (
                "scope-ended",
                "scope_ended",
                json!({"answer": "refused", "reason": "scope_ended"}),
            ),
            (
                "restart",
                "restart_before_dispatch",
                json!({"answer": "withdrawn"}),
            ),
        ] {
            let key = Key {
                carrier: "carrier".into(),
                call_key: call_key.into(),
            };
            admit(&conn, &key, "digest", stamp, State::Prepared).unwrap();
            settle(
                &conn,
                &key,
                &not_started(&key, reason),
                "not_started",
                Some(reason),
                100,
            )
            .unwrap();
            // Expiry before the first withdraw loses nothing of the answer.
            assert_eq!(
                serde_json::from_slice::<Value>(&withdraw_bytes(
                    &conn,
                    &key,
                    stamp,
                    100 + RETENTION_MS
                ))
                .unwrap(),
                expected,
                "{call_key}"
            );
        }

        // Started, end unobserved: already_started unknown, never completed,
        // identically on repeat and after expiry.
        let unknown = Key {
            carrier: "carrier".into(),
            call_key: "unknown".into(),
        };
        admit(&conn, &unknown, "digest", stamp, State::DispatchStarted).unwrap();
        recover(&conn).unwrap();
        let first = withdraw_bytes(&conn, &unknown, stamp, 100);
        assert_eq!(
            serde_json::from_slice::<Value>(&first).unwrap(),
            json!({"answer": "already_started", "outcome": "unknown"})
        );
        assert_eq!(
            withdraw_bytes(&conn, &unknown, stamp, now_ms() + RETENTION_MS),
            first
        );

        // Ran: completed with the recorded reply, frozen across expiry.
        let ran = Key {
            carrier: "carrier".into(),
            call_key: "ran".into(),
        };
        admit(&conn, &ran, "digest", stamp, State::DispatchStarted).unwrap();
        settle(&conn, &ran, &success(), "ok", None, 100).unwrap();
        let first = withdraw_bytes(&conn, &ran, stamp, 100);
        assert_eq!(
            serde_json::from_slice::<Value>(&first).unwrap(),
            json!({"answer": "completed", "result": serde_json::from_slice::<Value>(&success().body).unwrap(), "result_retained": true})
        );
        assert_eq!(
            withdraw_bytes(&conn, &ran, stamp, 100 + RETENTION_MS),
            first
        );
        assert_eq!(
            get(&conn, &ran).unwrap().unwrap().frame.unwrap().body,
            not_retained(&ran, "ok").body
        );

        // Ran, first withdrawn after expiry: the expired form is frozen.
        let late = Key {
            carrier: "carrier".into(),
            call_key: "late".into(),
        };
        admit(&conn, &late, "digest", stamp, State::DispatchStarted).unwrap();
        settle(&conn, &late, &success(), "ok", None, 100).unwrap();
        let first = withdraw_bytes(&conn, &late, stamp, 100 + RETENTION_MS);
        assert_eq!(
            serde_json::from_slice::<Value>(&first).unwrap(),
            json!({"answer": "completed", "outcome": "ok", "result_retained": false})
        );
        assert_eq!(
            withdraw_bytes(&conn, &late, stamp, 200 + RETENTION_MS),
            first
        );

        // No record, and a record under another scope: neither freezes.
        let missing = Key {
            carrier: "carrier".into(),
            call_key: "missing".into(),
        };
        assert_eq!(
            withdraw(&conn, &missing, stamp, 100, usize::MAX).unwrap(),
            Withdrawal::UnknownCall
        );
        let elsewhere = Key {
            carrier: "carrier".into(),
            call_key: "elsewhere".into(),
        };
        admit(&conn, &elsewhere, "digest", stamp, State::Prepared).unwrap();
        let other_scope = ScopeIdentity {
            scope_ref: "other".into(),
            ..scope.clone()
        };
        assert_eq!(
            withdraw(&conn, &elsewhere, Some(&other_scope), 100, usize::MAX).unwrap(),
            Withdrawal::NotPermitted
        );
        assert_eq!(
            withdraw(&conn, &elsewhere, None, 100, usize::MAX).unwrap(),
            Withdrawal::NotPermitted
        );
        let row = get(&conn, &elsewhere).unwrap().unwrap();
        assert_eq!(row.state, State::Prepared);
        assert!(row.withdraw_answer.is_none());
    }

    #[test]
    fn a_completed_answer_too_large_for_one_frame_is_reduced_with_its_row() {
        let conn = database();
        let scope = scope("owner");
        let key = key("carrier");
        admit(&conn, &key, "digest", Some(&scope), State::DispatchStarted).unwrap();
        settle(&conn, &key, &success(), "ok", None, 100).unwrap();
        let limit = answer_bytes(&settled_answer(&get(&conn, &key).unwrap().unwrap())).len() - 1;
        let first = match withdraw(&conn, &key, Some(&scope), 100, limit).unwrap() {
            Withdrawal::Answer { body, .. } => body,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            serde_json::from_slice::<Value>(&first).unwrap(),
            json!({"answer": "completed", "outcome": "ok", "result_retained": false})
        );
        let row = get(&conn, &key).unwrap().unwrap();
        assert_eq!(row.frame.unwrap().body, not_retained(&key, "ok").body);
        let entry: Value = serde_json::from_str(row.late_entry.as_deref().unwrap()).unwrap();
        assert_eq!(
            (
                entry["kind"].as_str(),
                entry["reduced"].as_bool(),
                entry["outcome"].as_str()
            ),
            (Some("result"), Some(true), Some("ok"))
        );
        assert!(entry.get("result").is_none());
    }

    fn page(
        conn: &Connection,
        custodian: &str,
        since: Option<u64>,
        limit: Option<u32>,
        now: i64,
        max_body: usize,
    ) -> Value {
        let since = since.map(|seq| Cursor {
            provider_incarnation: "incarnation".into(),
            seq,
        });
        serde_json::from_slice(
            &late_results(
                conn,
                custodian,
                "incarnation",
                since.as_ref(),
                limit,
                now,
                max_body,
            )
            .unwrap()
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn every_settlement_yields_its_late_entry_once_and_only_to_the_owner() {
        let conn = database();
        let scope = scope("owner");
        let stamp = Some(&scope);
        let make = |call_key: &str, state| {
            let key = Key {
                carrier: "carrier".into(),
                call_key: call_key.into(),
            };
            admit(&conn, &key, "digest", stamp, state).unwrap();
            key
        };
        // Recovery also sweeps against the wall clock, so settle at it.
        let now = now_ms();
        let ran = make("ran", State::DispatchStarted);
        settle(&conn, &ran, &success(), "ok", None, now).unwrap();
        let expired = make("expired", State::DispatchStarted);
        settle(&conn, &expired, &success(), "ok", None, now).unwrap();
        let withdrawn = make("withdrawn", State::Prepared);
        withdraw(&conn, &withdrawn, stamp, now, usize::MAX).unwrap();
        for reason in ["denied", "expired"] {
            let key = make(&format!("ask-{reason}"), State::Prepared);
            settle(
                &conn,
                &key,
                &success(),
                "bash_denied_untrusted",
                Some(reason),
                now,
            )
            .unwrap();
        }
        make("restart", State::Authorized);
        make("unobserved", State::DispatchStarted);
        recover(&conn).unwrap();
        // One result settled a retention period ago; everything else is fresh.
        conn.execute(
            "UPDATE call_ledger SET settled_at=?1 WHERE call_key='expired'",
            [now - RETENTION_MS],
        )
        .unwrap();
        let reply = page(&conn, "owner", None, None, now, usize::MAX);
        assert_eq!(reply["more"], false);
        let mut seen: Vec<(String, String, Option<String>, Option<bool>)> = reply["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                assert_eq!(entry["custodian"], "owner");
                assert_eq!(entry["owner"], "owner");
                (
                    entry["call_key"].as_str().unwrap().to_owned(),
                    entry["kind"].as_str().unwrap().to_owned(),
                    entry["reason"].as_str().map(str::to_owned),
                    entry["reduced"].as_bool(),
                )
            })
            .collect();
        seen.sort();
        let s = |value: &str| value.to_owned();
        assert_eq!(
            seen,
            vec![
                (
                    s("ask-denied"),
                    s("not_started"),
                    Some(s("denied")),
                    Some(false)
                ),
                (
                    s("ask-expired"),
                    s("not_started"),
                    Some(s("expired")),
                    Some(false)
                ),
                (s("expired"), s("expired"), None, Some(true)),
                (s("ran"), s("result"), None, Some(false)),
                (
                    s("restart"),
                    s("not_started"),
                    Some(s("restart_before_dispatch")),
                    Some(false)
                ),
                (s("unobserved"), s("result"), None, Some(true)),
                (
                    s("withdrawn"),
                    s("not_started"),
                    Some(s("withdrawn")),
                    Some(false)
                ),
            ]
        );
        let unobserved = reply["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["call_key"] == "unobserved")
            .unwrap();
        assert_eq!(unobserved["outcome"], "unknown");
        // Late entries go only to the scope's owner; a carrier that is not
        // the owner pages an empty list.
        assert_eq!(
            page(&conn, "carrier", None, None, now, usize::MAX)["entries"],
            json!([])
        );
        // Reading on from the cursor returns nothing again; acking drops all.
        let cursor = reply["cursor"]["seq"].as_u64().unwrap();
        assert_eq!(
            page(&conn, "owner", Some(cursor), None, now, usize::MAX)["entries"],
            json!([])
        );
        let through = Cursor {
            provider_incarnation: "incarnation".into(),
            seq: cursor,
        };
        assert_eq!(
            ack_through(&conn, "owner", "incarnation", &through)
                .unwrap()
                .unwrap(),
            7
        );
        assert_eq!(
            page(&conn, "owner", None, None, now, usize::MAX)["entries"],
            json!([])
        );
    }

    #[test]
    fn late_results_pages_stop_at_the_frame_limit_and_reduce_an_oversized_entry() {
        let conn = database();
        let scope = scope("owner");
        let big = |n: usize| RecordedFrame {
            ty: FrameType::Response,
            body: serde_json::to_vec(
                &json!({"isError": false, "content": [{"type": "text", "text": "x".repeat(n)}]}),
            )
            .unwrap(),
        };
        for (call_key, size) in [("a", 300), ("b", 300), ("c", 5_000)] {
            let key = Key {
                carrier: "carrier".into(),
                call_key: call_key.into(),
            };
            admit(&conn, &key, "digest", Some(&scope), State::DispatchStarted).unwrap();
            settle(&conn, &key, &big(size), "ok", None, 100).unwrap();
        }
        let full = page(&conn, "owner", None, None, 100, usize::MAX);
        let encoded = |value: &Value| serde_json::to_vec(value).unwrap().len();
        let one = page(&conn, "owner", None, Some(1), 100, usize::MAX);
        assert_eq!(one["more"], true);
        let limit = encoded(&one) + 100;
        // `a` fits, `a` and `b` together do not: the page stops before `b`.
        let first = page(&conn, "owner", None, None, 100, limit);
        assert_eq!(first["more"], true);
        assert_eq!(first["entries"].as_array().unwrap().len(), 1);
        assert_eq!(first["entries"][0]["call_key"], "a");
        assert!(encoded(&first) <= limit);
        let cursor = first["cursor"]["seq"].as_u64();
        let second = page(&conn, "owner", cursor, None, 100, limit);
        assert_eq!(second["entries"][0]["call_key"], "b");
        assert_eq!(second["more"], true);
        // Entry `c` exceeds the limit on its own: it is served with its
        // `result` replaced by `outcome` (`reduced: true`), and its ledger row
        // keeps only the outcome too.
        let third = page(
            &conn,
            "owner",
            second["cursor"]["seq"].as_u64(),
            None,
            100,
            limit,
        );
        assert_eq!(third["more"], false);
        let entry = &third["entries"][0];
        assert_eq!(
            (
                entry["call_key"].as_str(),
                entry["kind"].as_str(),
                entry["reduced"].as_bool(),
                entry["outcome"].as_str()
            ),
            (Some("c"), Some("result"), Some(true), Some("ok"))
        );
        assert!(entry.get("result").is_none());
        assert!(full["entries"][2].get("result").is_some());
        let c = Key {
            carrier: "carrier".into(),
            call_key: "c".into(),
        };
        assert_eq!(
            get(&conn, &c).unwrap().unwrap().frame.unwrap().body,
            not_retained(&c, "ok").body
        );
    }

    #[test]
    fn late_results_refuse_foreign_and_future_cursors_and_bound_retention() {
        let conn = database();
        let scope = scope("owner");
        for i in 0..UNACKED_RESULTS_MAX + 2 {
            let key = Key {
                carrier: "carrier".into(),
                call_key: format!("k{i}"),
            };
            admit(&conn, &key, "digest", Some(&scope), State::DispatchStarted).unwrap();
            settle(&conn, &key, &success(), "ok", None, 100).unwrap();
        }
        // The two oldest unacked results were dropped, rows included.
        assert!(get(
            &conn,
            &Key {
                carrier: "carrier".into(),
                call_key: "k0".into()
            }
        )
        .unwrap()
        .is_none());
        assert!(get(
            &conn,
            &Key {
                carrier: "carrier".into(),
                call_key: "k1".into()
            }
        )
        .unwrap()
        .is_none());
        assert!(get(
            &conn,
            &Key {
                carrier: "carrier".into(),
                call_key: "k2".into()
            }
        )
        .unwrap()
        .is_some());
        let head = (UNACKED_RESULTS_MAX + 2) as u64;
        for (incarnation, seq) in [("another", 1), ("incarnation", head + 1)] {
            let since = Cursor {
                provider_incarnation: incarnation.into(),
                seq,
            };
            let refused = late_results(
                &conn,
                "owner",
                "incarnation",
                Some(&since),
                None,
                100,
                usize::MAX,
            )
            .unwrap()
            .unwrap_err();
            assert_eq!(refused.code, "cursor_incarnation_changed");
            assert_eq!(
                ack_through(&conn, "owner", "incarnation", &since)
                    .unwrap()
                    .unwrap_err()
                    .code,
                "cursor_incarnation_changed"
            );
        }
        let first = page(&conn, "owner", None, None, 100, usize::MAX);
        assert_eq!(
            first["entries"].as_array().unwrap().len(),
            LATE_RESULTS_PAGE_MAX as usize
        );
        assert_eq!(first["entries"][0]["call_key"], "k2");
        assert_eq!(first["more"], true);
        // Entries past retention are served as expired, bounded per owner.
        let expired = page(
            &conn,
            "owner",
            None,
            Some(1),
            100 + RETENTION_MS,
            usize::MAX,
        );
        assert_eq!(expired["entries"][0]["kind"], "expired");
    }
}
