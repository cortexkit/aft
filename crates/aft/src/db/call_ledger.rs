//! Durable admission and settlement for keyed tool-provider calls.
//!
//! All operations use the actor's existing connection. State changes and late
//! entries share a transaction: a crash cannot publish half a settlement.
pub(crate) type HeldResultProjector =
    fn(&RecordedFrame, Option<&[u8]>, bool) -> Result<RecordedFrame, String>;
static HELD_RESULT_PROJECTOR: std::sync::OnceLock<HeldResultProjector> = std::sync::OnceLock::new();
pub(crate) fn register_held_result_projector(project: HeldResultProjector) {
    let _ = HELD_RESULT_PROJECTOR.set(project);
}
#[cfg(test)]
pub(crate) fn held_result_projector_registered() -> bool {
    HELD_RESULT_PROJECTOR.get().is_some()
}
pub(crate) fn project_held_result(
    frame: &RecordedFrame,
    source: Option<&[u8]>,
    structured: bool,
) -> Result<RecordedFrame, String> {
    HELD_RESULT_PROJECTOR
        .get()
        .ok_or_else(|| "held result projection unavailable".to_string())?(
        frame, source, structured
    )
}

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
/// owner. The carrier holds custody even when another principal owns the scope.
/// The oldest entries are dropped first.
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

/// Non-terminal feed entries have their own cursor positions without changing
/// the admitted call or consuming its later terminal answer. Acked rows retain
/// their deduplication key until the call is removed.
pub(super) const HELD_UPDATES_MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS call_ledger_updates (
    carrier TEXT NOT NULL, call_key TEXT NOT NULL, approval_key TEXT NOT NULL,
    ordinal INTEGER NOT NULL, custodian TEXT NOT NULL, seq INTEGER NOT NULL,
    event_id TEXT NOT NULL UNIQUE, settled_at INTEGER NOT NULL, entry TEXT,
    PRIMARY KEY(carrier,call_key,approval_key), UNIQUE(carrier,call_key,ordinal),
    FOREIGN KEY(carrier,call_key) REFERENCES call_ledger(carrier,call_key) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_call_ledger_updates_cursor ON call_ledger_updates(custodian,seq);
"#;

pub(crate) fn record_running_update(
    conn: &Connection,
    key: &Key,
    approval_key: &str,
    now: i64,
) -> rusqlite::Result<bool> {
    use cortexkit_role_tool_provider::{
        late_results::LateEntry,
        result::{held_reasons, HeldReply},
    };
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM call_ledger_updates WHERE carrier=?1 AND call_key=?2 AND approval_key=?3)", params![key.carrier,key.call_key,approval_key], |row| row.get(0))?;
    if exists {
        return Ok(false);
    }
    let row = get(&tx, key)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    let first_held = row.frame.as_ref().is_some_and(|frame| {
        serde_json::from_slice::<Value>(&frame.body)
            .is_ok_and(|body| body["held"]["reason"] == held_reasons::CONSENT)
    });
    if !first_held || row.state != State::Prepared {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    let scope = row.scope.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    let custodian = row.custodian.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    let ordinal: u64 = tx.query_row("SELECT COALESCE(MAX(ordinal),0)+1 FROM call_ledger_updates WHERE carrier=?1 AND call_key=?2", params![key.carrier,key.call_key], |row| row.get(0))?;
    let event_id = cortexkit_role_tool_provider::catalog::composition_digest(&json!([
        key.carrier,
        key.call_key,
        scope.owner,
        scope.scope_ref,
        scope.scope_epoch,
        held_reasons::RUNNING,
        ordinal
    ]))
    .expect("held event identity is canonical JSON");
    let entry = LateEntry::new_held(
        &scope.owner,
        &scope.scope_ref,
        scope.scope_epoch,
        &custodian,
        &key.call_key,
        format!("held-{event_id}"),
        now.max(0) as u64,
        HeldReply::new(held_reasons::RUNNING),
    );
    entry
        .validate()
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    tx.execute("INSERT INTO call_ledger_ack(custodian,last_seq) VALUES (?1,1) ON CONFLICT(custodian) DO UPDATE SET last_seq=last_seq+1", [&custodian])?;
    let seq = head_seq(&tx, &custodian)?;
    tx.execute("INSERT INTO call_ledger_updates(carrier,call_key,approval_key,ordinal,custodian,seq,event_id,settled_at,entry) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![key.carrier,key.call_key,approval_key,ordinal,custodian,seq,entry.event_id,now,serde_json::to_string(&entry).expect("held entry is JSON")])?;
    enforce_retention_bounds(&tx, &custodian)?;
    tx.commit()?;
    Ok(true)
}

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

pub(super) const CARRIER_CUSTODY_SCHEMA_VERSION: u32 = 29;

/// Relax the owner/custodian equality without changing any existing custody.
/// Save the child rows before dropping the old table: foreign-key cascades
/// would otherwise discard rows in `call_ledger_updates` during the rebuild.
pub(super) fn migrate_carrier_custody(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(r#"
        CREATE TABLE call_ledger_next (
            carrier TEXT NOT NULL, call_key TEXT NOT NULL, digest TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('Prepared','Authorized','DispatchStarted','Settled')),
            owner TEXT, scope_ref TEXT, scope_epoch INTEGER, custodian TEXT,
            task_id TEXT, withdraw_answer BLOB, frame_type TEXT, frame_body BLOB,
            outcome TEXT, event_id TEXT NOT NULL, settled_at INTEGER, seq INTEGER,
            acked INTEGER NOT NULL DEFAULT 0, late_entry TEXT, recovered INTEGER NOT NULL DEFAULT 0,
            tool_result_source BLOB CHECK(tool_result_source IS NULL OR
                (typeof(tool_result_source) = 'blob' AND length(tool_result_source) > 0
                    AND length(tool_result_source) < 3145728)),
            PRIMARY KEY(carrier,call_key),
            CHECK ((owner IS NULL AND scope_ref IS NULL AND scope_epoch IS NULL AND custodian IS NULL)
                OR (owner IS NOT NULL AND scope_ref IS NOT NULL AND scope_epoch IS NOT NULL AND custodian IS NOT NULL))
        );
        INSERT INTO call_ledger_next SELECT carrier,call_key,digest,state,owner,scope_ref,scope_epoch,custodian,
            task_id,withdraw_answer,frame_type,frame_body,outcome,event_id,settled_at,seq,acked,late_entry,recovered,tool_result_source FROM call_ledger;
        CREATE TEMP TABLE call_ledger_updates_saved AS SELECT * FROM call_ledger_updates;
        DROP TABLE call_ledger_updates;
        DROP TABLE call_ledger;
        ALTER TABLE call_ledger_next RENAME TO call_ledger;
        CREATE INDEX idx_call_ledger_retention ON call_ledger(state,settled_at);
        CREATE INDEX idx_call_ledger_custodian ON call_ledger(custodian,seq);
        CREATE INDEX idx_call_ledger_task ON call_ledger(task_id);
    "#)?;
    conn.execute_batch(HELD_UPDATES_MIGRATION)?;
    conn.execute_batch("INSERT INTO call_ledger_updates SELECT * FROM call_ledger_updates_saved; DROP TABLE call_ledger_updates_saved;")
}

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
    pub custodian: Option<String>,
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
        "SELECT digest,state,owner,scope_ref,scope_epoch,task_id,withdraw_answer,frame_type,frame_body,outcome,event_id,settled_at,seq,late_entry,custodian,tool_result_source FROM call_ledger WHERE carrier=?1 AND call_key=?2"
    } else {
        "SELECT digest,state,owner,scope_ref,scope_epoch,task_id,withdraw_answer,frame_type,frame_body,outcome,event_id,settled_at,seq,late_entry,custodian FROM call_ledger WHERE carrier=?1 AND call_key=?2"
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
            custodian: r.get(14)?,
            tool_result_source: if include_source { r.get(15)? } else { None },
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
        VALUES (?1,?2,?3,?4,?5,?6,?7,CASE WHEN ?5 IS NULL THEN NULL ELSE ?1 END,?8)",
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

/// A permission preflight did not dispatch the requested action. Return it to
/// the withdrawable state and retain its held-reply/v1 body before sending that
/// permission-wait reply to the program.
pub(crate) fn hold_permission(
    conn: &Connection,
    key: &Key,
    body: &[u8],
    include_structured: bool,
) -> rusqlite::Result<bool> {
    let mut retained: Value = serde_json::from_slice(body)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    retained["_aft_include_structured"] = json!(include_structured);
    Ok(conn.execute("UPDATE call_ledger SET state='Prepared',frame_type='Response',frame_body=?3 WHERE carrier=?1 AND call_key=?2 AND state IN ('Prepared','DispatchStarted') AND task_id IS NULL", params![key.carrier,key.call_key,retained.to_string().as_bytes()])? == 1)
}

/// DispatchStarted is the irrevocable start boundary, not native handler entry.
/// End fences installed before this transition refuse the call. Once committed,
/// the call is in flight: a later end must not settle it as not_started or undo
/// its handler. Scope cleanup still kills shell tasks belonging to that scope.
/// The transaction ends here; no authorization lock spans native dispatch.
pub(crate) fn begin_approved(conn: &Connection, key: &Key) -> rusqlite::Result<bool> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let Some(row) = get(&tx, key)?.filter(|row| row.state == State::Prepared) else {
        return Ok(false);
    };
    if let Some(scope) = &row.scope {
        let owner = match scope.owner.as_str() {
            "direct" => subc_protocol::Principal::Direct,
            "unverified" => subc_protocol::Principal::Unverified,
            other => match other.strip_prefix("reserved:") {
                Some(module_id) => subc_protocol::Principal::Reserved {
                    module_id: module_id.into(),
                },
                None => return Err(rusqlite::Error::InvalidQuery),
            },
        };
        let origin = super::access_grants::ScopeKey {
            owner: serde_json::to_string(&owner).expect("principal is JSON"),
            scope_ref: scope.scope_ref.clone(),
            epoch: scope.scope_epoch,
        };
        if super::access_grants::scope_ended(&tx, &origin)? {
            return Ok(false);
        }
    }
    let changed = transition(&tx, key, State::Prepared, State::Authorized)?
        && transition(&tx, key, State::Authorized, State::DispatchStarted)?;
    tx.commit()?;
    Ok(changed)
}

/// Retain a task-backed hold before replying. Keep a prior consent hold marker:
/// reason transitions use separate non-terminal entries, not another initial reply.
pub(crate) fn hold_running(
    conn: &Connection,
    key: &Key,
    task_id: &str,
    body: &[u8],
    include_structured: bool,
) -> rusqlite::Result<bool> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let row = get(&tx, key)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    let consent = row.frame.as_ref().is_some_and(|frame| {
        serde_json::from_slice::<Value>(&frame.body)
            .is_ok_and(|body| body["held"]["reason"] == "consent")
    });
    if row.state != State::DispatchStarted || row.task_id.as_deref() != Some(task_id) {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    let mut retained: Value = serde_json::from_slice(body)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    // Private retention metadata is never sent in the held reply. Remember the
    // original include_structured value so a restarted process knows whether
    // this caller may receive the response's structured data.
    retained["_aft_include_structured"] = json!(include_structured);
    let retained = retained.to_string().into_bytes();
    tx.execute("UPDATE call_ledger SET recovered=1,frame_type='Response',frame_body=?3 WHERE carrier=?1 AND call_key=?2", params![key.carrier,key.call_key, if consent { &row.frame.as_ref().unwrap().body } else { &retained }])?;
    tx.commit()?;
    Ok(!consent)
}

fn has_held_reply(row: &Row) -> bool {
    row.frame.as_ref().is_some_and(|frame| {
        serde_json::from_slice::<Value>(&frame.body).is_ok_and(|body| body.get("held").is_some())
    })
}

pub(crate) fn hold_watch(conn: &Connection, key: &Key, body: &[u8]) -> rusqlite::Result<()> {
    let changed = conn.execute("UPDATE call_ledger SET recovered=1,frame_type='Response',frame_body=?3 WHERE carrier=?1 AND call_key=?2 AND state='DispatchStarted'", params![key.carrier, key.call_key, body])?;
    if changed != 1 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    Ok(())
}

pub(crate) fn unknown_held_result() -> RecordedFrame {
    RecordedFrame {
        ty: FrameType::Response,
        body: serde_json::to_vec(
            &json!({"content":[{"type":"text","text":""}],"isError":true,"errorCode":"outcome_unknown"}),
        )
        .expect("unknown outcome is JSON"),
    }
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
    if response["success"] == true
        || value["isError"] == false
        || (value.get("content").is_some() && value.get("isError").is_none())
    {
        "ok".into()
    } else {
        value["errorCode"]
            .as_str()
            .or_else(|| response["code"].as_str())
            .unwrap_or("unknown")
            .into()
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
        let custodian = row
            .custodian
            .as_deref()
            .ok_or(rusqlite::Error::InvalidQuery)?;
        tx.execute("INSERT INTO call_ledger_ack(custodian,last_seq) VALUES (?1,1) ON CONFLICT(custodian) DO UPDATE SET last_seq=last_seq+1", [custodian])?;
        seq = Some(tx.query_row(
            "SELECT last_seq FROM call_ledger_ack WHERE custodian=?1",
            [custodian],
            |r| r.get::<_, u64>(0),
        )?);
        let mut value = json!({"kind":if reason.is_some() {"not_started"} else {"result"},"owner":scope.owner,"ref":scope.scope_ref,"scope_epoch":scope.scope_epoch,"custodian":custodian,"call_key":key.call_key,"event_id":row.event_id,"settled_at":now,"reduced":outcome == "unknown"});
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
    if let Some(custodian) = &row.custodian {
        enforce_retention_bounds(tx, custodian)?;
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
    let updates = tx.execute("UPDATE call_ledger_updates SET entry=NULL WHERE custodian=?1 AND seq<=?2 AND entry IS NOT NULL", params![custodian,through])?;
    tx.commit()?;
    Ok(deleted + updates)
}

/// Drops one custodian's oldest late entries beyond [`UNACKED_RESULTS_MAX`]
/// unacked results and [`EXPIRED_ENTRIES_MAX`] expired entries (the
/// tool-provider contract's retention bounds), deleting their rows as an ack
/// would, and clears held-update payloads beyond [`UNACKED_RESULTS_MAX`].
/// Every settle and held update runs this in its own transaction, so the
/// bounds hold after each of them; `late_results` runs it again before
/// reading, because expiry ([`reduce_expired`]) can push the expired count
/// over its bound between settles.
///
/// Its cost must not follow the size of the stored results, since a late
/// entry can carry a whole tool result. A custodian with no more rows than
/// either bound is skipped after reading only its index. Otherwise one pass
/// visits each late entry, newest first, and reads its frame type from the
/// row's first page. Only a row with an `Error` frame can hold an `expired`
/// entry, because [`reduce_expired`] writes that frame and that kind in one
/// update; every other late entry counts as unexpired without being read.
/// The entries of `Error` rows are small (a not-started, not-retained or
/// protocol error body) and are classified by their `kind`, with the same
/// SQL comparison as before, so an entry whose kind is missing still counts
/// against neither bound.
fn enforce_retention_bounds(conn: &Connection, custodian: &str) -> rusqlite::Result<()> {
    let fewest_kept = UNACKED_RESULTS_MAX.min(EXPIRED_ENTRIES_MAX) as i64;
    let over_either_bound = conn
        .query_row(
            "SELECT 1 FROM call_ledger WHERE custodian=?1 LIMIT 1 OFFSET ?2",
            params![custodian, fewest_kept],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if over_either_bound {
        drop_late_entries_over_bounds(conn, custodian)?;
    }
    let over_update_bound = conn
        .query_row(
            "SELECT 1 FROM call_ledger_updates WHERE custodian=?1 LIMIT 1 OFFSET ?2",
            params![custodian, UNACKED_RESULTS_MAX as i64],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if over_update_bound {
        conn.execute("UPDATE call_ledger_updates SET entry=NULL WHERE rowid IN (SELECT rowid FROM call_ledger_updates WHERE custodian=?1 AND entry IS NOT NULL ORDER BY seq DESC LIMIT -1 OFFSET ?2)", params![custodian,UNACKED_RESULTS_MAX as i64])?;
    }
    Ok(())
}

/// The row deletions of [`enforce_retention_bounds`]: newest first, the
/// unexpired entries past the first [`UNACKED_RESULTS_MAX`] and the expired
/// entries past the first [`EXPIRED_ENTRIES_MAX`].
fn drop_late_entries_over_bounds(conn: &Connection, custodian: &str) -> rusqlite::Result<()> {
    let over_bounds = {
        // `late_entry IS NOT NULL` reads only the row header, and the CASE
        // reads the entry itself only for `Error` rows.
        let mut stmt = conn.prepare(
            "SELECT rowid, CASE WHEN frame_type='Error'
                 THEN json_extract(late_entry,'$.kind')='expired' ELSE 0 END
             FROM call_ledger WHERE custodian=?1 AND late_entry IS NOT NULL
             ORDER BY seq DESC",
        )?;
        let mut rows = stmt.query([custodian])?;
        let (mut unexpired, mut expired) = (0, 0);
        let mut over_bounds = Vec::new();
        while let Some(row) = rows.next()? {
            let kept = match row.get::<_, Option<bool>>(1)? {
                Some(false) => {
                    unexpired += 1;
                    unexpired <= UNACKED_RESULTS_MAX
                }
                Some(true) => {
                    expired += 1;
                    expired <= EXPIRED_ENTRIES_MAX
                }
                None => true,
            };
            if !kept {
                over_bounds.push(row.get::<_, i64>(0)?);
            }
        }
        over_bounds
    };
    let mut delete = conn.prepare("DELETE FROM call_ledger WHERE rowid=?1")?;
    for rowid in over_bounds {
        delete.execute([rowid])?;
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
    late_results_for_consumer(
        conn,
        custodian,
        incarnation,
        since,
        limit,
        now,
        max_body,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn late_results_for_consumer(
    conn: &Connection,
    custodian: &str,
    incarnation: &str,
    since: Option<&Cursor>,
    limit: Option<u32>,
    now: i64,
    max_body: usize,
    held_updates: bool,
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
            "SELECT carrier,call_key,seq,0 FROM call_ledger
             WHERE custodian=?1 AND seq>?2 AND late_entry IS NOT NULL
             UNION ALL SELECT carrier,call_key,seq,1 FROM call_ledger_updates
             WHERE ?4 AND custodian=?1 AND seq>?2 AND entry IS NOT NULL ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                params![custodian, since_seq, cap as i64 + 1, held_updates],
                |r| {
                    Ok((
                        Key {
                            carrier: r.get(0)?,
                            call_key: r.get(1)?,
                        },
                        r.get::<_, u64>(2)?,
                        r.get::<_, bool>(3)?,
                    ))
                },
            )?
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
    for (key, seq, update) in rows.into_iter().take(cap) {
        if update {
            let (raw, effective_at): (String, i64) = tx.query_row("SELECT entry,settled_at FROM call_ledger_updates WHERE carrier=?1 AND call_key=?2 AND seq=?3", params![key.carrier,key.call_key,seq], |row| Ok((row.get(0)?,row.get(1)?)))?;
            let update: cortexkit_role_tool_provider::late_results::LateEntry =
                serde_json::from_str(&raw)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            update
                .validate()
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            let entry = serde_json::to_value(update).expect("validated held update is JSON");
            let len = serde_json::to_vec(&entry)
                .expect("held entry is JSON")
                .len();
            if effective_at.saturating_add(RETENTION_MS) <= now || overhead + len > max_body {
                tx.execute("UPDATE call_ledger_updates SET entry=NULL WHERE carrier=?1 AND call_key=?2 AND seq=?3", params![key.carrier,key.call_key,seq])?;
                last = seq;
                continue;
            }
            let added = len + usize::from(!entries.is_empty());
            if size + added > max_body {
                more = true;
                break;
            }
            size += added;
            last = seq;
            entries.push(entry);
            continue;
        }
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
        if row.frame.as_ref().is_some_and(|frame| {
            serde_json::from_slice::<Value>(&frame.body)
                .is_ok_and(|body| body.get("_aft_restart_watch").is_some())
        }) {
            // The watched task belongs to a different call. Its saved wait is
            // resumed by the subc layer; this recovery pass skips it.
            continue;
        }
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
                if has_held_reply(&row) {
                    settle(
                        conn,
                        &key,
                        &unknown_held_result(),
                        "outcome_unknown",
                        None,
                        now_ms(),
                    )?;
                    continue;
                }
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
        let row = get(conn, &key)?;
        if row.as_ref().is_some_and(has_held_reply) {
            let frame = if outcome == "unknown" {
                unknown_held_result()
            } else {
                let structured = row
                    .as_ref()
                    .and_then(|row| row.frame.as_ref())
                    .and_then(|frame| serde_json::from_slice::<Value>(&frame.body).ok())
                    .is_some_and(|held| held["_aft_include_structured"] == true);
                project_held_result(frame, body, structured).map_err(|error| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(error)))
                })?
            };
            return settle_with_tool_result_source(
                conn,
                &key,
                &frame,
                &self::outcome(&frame),
                None,
                now_ms(),
                body,
            )
            .map(|_| true);
        }
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
    fn schema_29_preserves_schema_28_rows_and_indexes_but_allows_carrier_custody() {
        use super::*;
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::run_migrations_supported(&mut conn, 28).unwrap();
        conn.execute_batch("INSERT INTO call_ledger(carrier,call_key,digest,state,owner,scope_ref,scope_epoch,custodian,task_id,withdraw_answer,frame_type,frame_body,outcome,event_id,settled_at,seq,acked,late_entry,recovered,tool_result_source)
            VALUES ('broca','old','digest','DispatchStarted','core','worker',7,'core','task',x'0102','Response',x'0304','ok','event',123,4,1,'entry',1,x'7b7d');
            INSERT INTO call_ledger(carrier,call_key,digest,state,event_id) VALUES ('broca','unscoped','other','Authorized','other-event');
            INSERT INTO call_ledger_ack(custodian,through_seq,last_seq) VALUES ('core',3,5);
            INSERT INTO call_ledger_updates(carrier,call_key,approval_key,ordinal,custodian,seq,event_id,settled_at,entry) VALUES ('broca','old','approval',1,'core',5,'update',122,'saved-update');").unwrap();
        let snapshot = |conn: &Connection, table: &str| {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
                .unwrap();
            stmt.query_map([], |row| {
                (0..row.as_ref().column_count())
                    .map(|i| row.get::<_, rusqlite::types::Value>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        let tables = ["call_ledger", "call_ledger_updates", "call_ledger_ack"];
        let before: Vec<_> = tables.iter().map(|table| snapshot(&conn, table)).collect();
        crate::db::run_migrations(&mut conn).unwrap();
        assert_eq!(crate::db::current_schema_version(&conn).unwrap(), 29);
        for (table, before) in tables.iter().zip(before) {
            assert_eq!(
                snapshot(&conn, table),
                before,
                "{table} changed during migration"
            );
        }
        let indexes: usize = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='index' AND name IN ('idx_call_ledger_retention','idx_call_ledger_custodian','idx_call_ledger_task','idx_call_ledger_updates_cursor')", [], |row| row.get(0)).unwrap();
        assert_eq!(indexes, 4);
        let key = Key {
            carrier: "broca".into(),
            call_key: "new".into(),
        };
        let scope = ScopeIdentity {
            owner: "core".into(),
            scope_ref: "worker".into(),
            scope_epoch: 7,
        };
        admit(&conn, &key, "new", Some(&scope), State::DispatchStarted).unwrap();
        assert_eq!(
            get(&conn, &key).unwrap().unwrap().custodian.as_deref(),
            Some("broca")
        );
        assert_eq!(
            get(
                &conn,
                &Key {
                    carrier: "broca".into(),
                    call_key: "old".into()
                }
            )
            .unwrap()
            .unwrap()
            .custodian
            .as_deref(),
            Some("core")
        );
    }

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
    fn approved_dispatch_checks_ended_scope_even_when_settlement_left_it_prepared() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let scope = ScopeIdentity {
            owner: "reserved:prefrontal-core".into(),
            scope_ref: "run:ended".into(),
            scope_epoch: 7,
        };
        let key = Key {
            carrier: "reserved:basal".into(),
            call_key: "approved".into(),
        };
        admit(&conn, &key, "digest", Some(&scope), State::Prepared).unwrap();
        let origin = crate::db::access_grants::ScopeKey {
            owner: serde_json::to_string(&subc_protocol::Principal::Reserved {
                module_id: "prefrontal-core".into(),
            })
            .unwrap(),
            scope_ref: scope.scope_ref.clone(),
            epoch: scope.scope_epoch,
        };
        crate::db::access_grants::end_scope_grants(&conn, &origin).unwrap();
        assert!(!begin_approved(&conn, &key).unwrap());
        assert_eq!(get(&conn, &key).unwrap().unwrap().state, State::Prepared);
        let live_scope = ScopeIdentity {
            scope_ref: "run:live".into(),
            ..scope
        };
        let live = Key {
            call_key: "live".into(),
            ..key
        };
        admit(&conn, &live, "digest", Some(&live_scope), State::Prepared).unwrap();
        assert!(begin_approved(&conn, &live).unwrap());
        assert_eq!(
            get(&conn, &live).unwrap().unwrap().state,
            State::DispatchStarted
        );
    }

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
    fn runner_restart_held_shell_recovers_exact_terminal_bytes_or_explicit_unknown() {
        crate::subc::lower_hooks::register();
        let conn = database();
        let scope = scope("core");
        let held = serde_json::to_vec(&json!({"content":[{"type":"text","text":""}],"held":{"reason":"running","yielded":true}})).unwrap();
        for (carrier, task) in [("broca", "live-task"), ("lost", "missing-task")] {
            let key = key(carrier);
            admit(&conn, &key, "digest", Some(&scope), State::DispatchStarted).unwrap();
            link_task(&conn, &key, task).unwrap();
            hold_running(&conn, &key, task, &held, false).unwrap();
        }
        let key = key("broca");
        conn.execute("INSERT INTO bash_tasks(harness,session_id,task_id,project_key,command,cwd,status,started_at,metadata) VALUES('h','s','live-task','p','command','cwd','running',1,?1)", [json!({"call_key":{"requester":key.carrier,"key":key.call_key}}).to_string()]).unwrap();
        recover(&conn).unwrap();
        assert_eq!(
            get(&conn, &key).unwrap().unwrap().state,
            State::DispatchStarted
        );
        let lost = get(&conn, &super::tests::key("lost")).unwrap().unwrap();
        assert_eq!(lost.state, State::Settled);
        assert_eq!(lost.frame.unwrap().body, unknown_held_result().body);
        let ordinary = RecordedFrame {
            ty: FrameType::Response,
            body: serde_json::to_vec(
                &json!({"content":[{"type":"text","text":"real command output"}],"isError":false}),
            )
            .unwrap(),
        };
        assert!(observe_task(
            &conn,
            &key.carrier,
            &key.call_key,
            "live-task",
            Some(&ordinary),
            "ok"
        )
        .unwrap());
        assert!(!observe_task(
            &conn,
            &key.carrier,
            &key.call_key,
            "live-task",
            Some(&ordinary),
            "ok"
        )
        .unwrap());
        let row = get(&conn, &key).unwrap().unwrap();
        assert_eq!(row.frame.unwrap().body, ordinary.body);
        assert_eq!(row.seq, Some(1));
        assert_eq!(
            page(&conn, "core", None, None, now_ms(), usize::MAX)["entries"],
            json!([])
        );
        assert_eq!(
            page(&conn, "broca", None, None, now_ms(), usize::MAX)["entries"][0]["result"],
            serde_json::from_slice::<Value>(&ordinary.body).unwrap()
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
    fn every_settlement_yields_its_late_entry_once_and_only_to_the_carrier() {
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
        let reply = page(&conn, "carrier", None, None, now, usize::MAX);
        assert_eq!(reply["more"], false);
        let mut seen: Vec<(String, String, Option<String>, Option<bool>)> = reply["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                assert_eq!(entry["custodian"], "carrier");
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
        // Late entries go only to the carrier; the scope's owner cannot collect them.
        assert_eq!(
            page(&conn, "owner", None, None, now, usize::MAX)["entries"],
            json!([])
        );
        // Reading on from the cursor returns nothing again; acking drops all.
        let cursor = reply["cursor"]["seq"].as_u64().unwrap();
        assert_eq!(
            page(&conn, "carrier", Some(cursor), None, now, usize::MAX)["entries"],
            json!([])
        );
        let through = Cursor {
            provider_incarnation: "incarnation".into(),
            seq: cursor,
        };
        assert_eq!(
            ack_through(&conn, "carrier", "incarnation", &through)
                .unwrap()
                .unwrap(),
            7
        );
        assert_eq!(
            page(&conn, "carrier", None, None, now, usize::MAX)["entries"],
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
        let full = page(&conn, "carrier", None, None, 100, usize::MAX);
        let encoded = |value: &Value| serde_json::to_vec(value).unwrap().len();
        let one = page(&conn, "carrier", None, Some(1), 100, usize::MAX);
        assert_eq!(one["more"], true);
        let limit = encoded(&one) + 100;
        // `a` fits, `a` and `b` together do not: the page stops before `b`.
        let first = page(&conn, "carrier", None, None, 100, limit);
        assert_eq!(first["more"], true);
        assert_eq!(first["entries"].as_array().unwrap().len(), 1);
        assert_eq!(first["entries"][0]["call_key"], "a");
        assert!(encoded(&first) <= limit);
        let cursor = first["cursor"]["seq"].as_u64();
        let second = page(&conn, "carrier", cursor, None, 100, limit);
        assert_eq!(second["entries"][0]["call_key"], "b");
        assert_eq!(second["more"], true);
        // Entry `c` exceeds the limit on its own: it is served with its
        // `result` replaced by `outcome` (`reduced: true`), and its ledger row
        // keeps only the outcome too.
        let third = page(
            &conn,
            "carrier",
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
                "carrier",
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
                ack_through(&conn, "carrier", "incarnation", &since)
                    .unwrap()
                    .unwrap_err()
                    .code,
                "cursor_incarnation_changed"
            );
        }
        let first = page(&conn, "carrier", None, None, 100, usize::MAX);
        assert_eq!(
            first["entries"].as_array().unwrap().len(),
            LATE_RESULTS_PAGE_MAX as usize
        );
        assert_eq!(first["entries"][0]["call_key"], "k2");
        assert_eq!(first["more"], true);
        // Entries past retention are served as expired, bounded per owner.
        let expired = page(
            &conn,
            "carrier",
            None,
            Some(1),
            100 + RETENTION_MS,
            usize::MAX,
        );
        assert_eq!(expired["entries"][0]["kind"], "expired");
    }

    fn text_result(text: &str) -> RecordedFrame {
        RecordedFrame {
            ty: FrameType::Response,
            body: serde_json::to_vec(&json!({
                "isError": false,
                "content": [{"type": "text", "text": text}],
            }))
            .unwrap(),
        }
    }

    /// Pad characters that add exactly one overflow page to a settled row. A
    /// result's text is stored twice, in the recorded body and in the late
    /// entry, and an overflow page of a 4096-byte database page holds 4092
    /// bytes. Rows whose sizes differ by whole overflow pages keep identical
    /// bytes on their table page, so ledgers built with such pads have the
    /// same tree shape and differ only in each result's overflow chain.
    const PAD_PER_OVERFLOW_PAGE: usize = 2046;

    /// Builds an on-disk ledger for the custodian `owner`: `expired` expired
    /// entries with the oldest seqs, then `results` unexpired results that
    /// each carry `text`. The rows are copies of rows that `settle` and
    /// `reduce_expired` wrote. Returns how many overflow pages the ledger
    /// table uses.
    fn build_ledger_file(
        path: &std::path::Path,
        results: usize,
        expired: usize,
        text: &str,
    ) -> i64 {
        let conn = crate::db::open(path).unwrap();
        let page_size: i64 = conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            page_size, 4096,
            "PAD_PER_OVERFLOW_PAGE assumes 4096-byte pages"
        );
        // The templates belong to another custodian, so they never count
        // against the bounds of `owner`.
        let template = scope("template");
        for name in ["template-result", "template-expired"] {
            admit(
                &conn,
                &key(name),
                "digest",
                Some(&template),
                State::DispatchStarted,
            )
            .unwrap();
        }
        settle(
            &conn,
            &key("template-result"),
            &text_result(text),
            "ok",
            None,
            100,
        )
        .unwrap();
        settle(&conn, &key("template-expired"), &success(), "ok", None, 100).unwrap();
        reduce_expired(&conn, &key("template-expired"), 100 + RETENTION_MS).unwrap();
        for (template, prefix, first_seq, count) in [
            ("template-expired", "expired-", 0, expired),
            ("template-result", "result-", expired, results),
        ] {
            if count == 0 {
                continue;
            }
            conn.execute(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<?4)
                 INSERT INTO call_ledger(carrier,call_key,digest,state,owner,scope_ref,scope_epoch,
                     custodian,frame_type,frame_body,outcome,event_id,settled_at,seq,late_entry)
                 SELECT 'carrier',?2||i,digest,state,'owner',scope_ref,scope_epoch,'owner',
                     frame_type,frame_body,outcome,?2||i,settled_at,?3+i,late_entry
                 FROM call_ledger, n WHERE carrier=?1",
                params![template, prefix, first_seq as i64, count as i64],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO call_ledger_ack(custodian,last_seq) VALUES ('owner',?1)",
            [(expired + results) as i64],
        )
        .unwrap();
        conn.query_row(
            "SELECT count(*) FROM dbstat WHERE name='call_ledger' AND pagetype='overflow'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[derive(Debug)]
    struct SettleCost {
        /// Pages the settle pulled from the database file.
        pages_read: i32,
        /// Late entries of kind `result` that SQL parsed during the settle.
        result_entries_parsed: usize,
    }

    /// Settles one more result for `owner` on a fresh connection to `path`,
    /// whose page cache starts empty, and reports what that settle read.
    fn cold_settle_cost(path: &std::path::Path) -> SettleCost {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let conn = crate::db::open(path).unwrap();
        // Large enough that no page is evicted and read a second time.
        conn.pragma_update(None, "cache_size", -262_144).unwrap();
        // An application function overrides SQLite's built-in json_extract
        // on this connection, so every entry SQL parses is counted here.
        let parsed = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = parsed.clone();
        conn.create_scalar_function(
            "json_extract",
            2,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            move |ctx| {
                let entry: String = ctx.get(0)?;
                let path: String = ctx.get(1)?;
                let member = path
                    .strip_prefix("$.")
                    .expect("the ledger reads top-level members");
                let value: Value = serde_json::from_str(&entry)
                    .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
                let found = value.get(member).cloned().unwrap_or(Value::Null);
                if member == "kind" && found == "result" {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                Ok(found.as_str().map(str::to_owned))
            },
        )
        .unwrap();
        let kind: String = conn
            .query_row(
                r#"SELECT json_extract('{"kind":"result"}','$.kind')"#,
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kind, "result");
        assert_eq!(
            parsed.swap(0, Ordering::SeqCst),
            1,
            "SQL must call the counting json_extract"
        );
        let next = Key {
            carrier: "owner".into(),
            call_key: "next".into(),
        };
        admit(
            &conn,
            &next,
            "digest",
            Some(&scope("owner")),
            State::DispatchStarted,
        )
        .unwrap();
        let cache_misses = |reset: bool| {
            let (mut current, mut highwater) = (0, 0);
            let status = unsafe {
                rusqlite::ffi::sqlite3_db_status(
                    conn.handle(),
                    rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS,
                    &mut current,
                    &mut highwater,
                    i32::from(reset),
                )
            };
            assert_eq!(status, rusqlite::ffi::SQLITE_OK);
            current
        };
        cache_misses(true);
        settle(&conn, &next, &success(), "ok", None, 100).unwrap();
        SettleCost {
            pages_read: cache_misses(false),
            result_entries_parsed: parsed.load(Ordering::SeqCst),
        }
    }

    /// A settle enforces the retention bounds over all of the custodian's late
    /// entries. Its reads must not depend on how large the stored results
    /// are: each unexpired result here is either about 8 KB or 18 KB, and the
    /// settle must read exactly the same pages and parse none of them.
    #[test]
    fn settle_cost_does_not_depend_on_the_size_of_stored_results() {
        const RESULTS: usize = 200;
        // Keeps every result's body and entry above 8,185 bytes, so their
        // length prefixes in the row header have the same width in both sizes.
        const SMALL_PAD: usize = 8_300;
        const ADDED_PAGES: usize = 5;
        let dir = tempfile::tempdir().unwrap();
        // Fewer late entries than either bound, then more than the bounds
        // allow: one expired entry too many, which this settle must drop.
        for expired in [0, EXPIRED_ENTRIES_MAX + 1] {
            let mut measured = Vec::new();
            for (size, pad) in [
                ("small", SMALL_PAD),
                ("large", SMALL_PAD + ADDED_PAGES * PAD_PER_OVERFLOW_PAGE),
            ] {
                let path = dir.path().join(format!("{size}-{expired}.db"));
                let overflow_pages = build_ledger_file(&path, RESULTS, expired, &"x".repeat(pad));
                let cost = cold_settle_cost(&path);
                let conn = crate::db::open(&path).unwrap();
                let owner_rows: usize = conn
                    .query_row(
                        "SELECT count(*) FROM call_ledger WHERE custodian='owner'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    owner_rows,
                    RESULTS + 1 + expired.min(EXPIRED_ENTRIES_MAX),
                    "{size}: the settle kept every result and dropped only the expired entry over its bound"
                );
                if expired > EXPIRED_ENTRIES_MAX {
                    let oldest = Key {
                        carrier: "carrier".into(),
                        call_key: "expired-1".into(),
                    };
                    assert!(get(&conn, &oldest).unwrap().is_none(), "{size}");
                }
                measured.push((overflow_pages, cost));
            }
            let [(small_overflow, small), (large_overflow, large)] = &measured[..] else {
                unreachable!()
            };
            // Every result row, the template included, really is larger.
            assert_eq!(
                large_overflow - small_overflow,
                ((RESULTS + 1) * ADDED_PAGES) as i64
            );
            assert_eq!(
                (small.result_entries_parsed, large.result_entries_parsed),
                (0, 0),
                "a settle parsed stored result entries ({expired} expired)"
            );
            assert_eq!(
                small.pages_read, large.pages_read,
                "settle page reads grew with the size of stored results ({expired} expired): {small:?} vs {large:?}"
            );
        }
    }

    /// The retention query every settle ran before the bounds stopped parsing
    /// result entries. The test below checks that the current enforcement
    /// drops exactly the rows this query drops.
    fn drop_over_bounds_by_parsing_every_entry(conn: &Connection, custodian: &str) {
        for (expired, max) in [(false, UNACKED_RESULTS_MAX), (true, EXPIRED_ENTRIES_MAX)] {
            conn.execute(
                "DELETE FROM call_ledger WHERE rowid IN (SELECT rowid FROM call_ledger
                    WHERE custodian=?1 AND late_entry IS NOT NULL
                    AND (json_extract(late_entry,'$.kind')='expired')=?2
                    ORDER BY seq DESC LIMIT -1 OFFSET ?3)",
                params![custodian, expired, max as i64],
            )
            .unwrap();
        }
    }

    #[test]
    fn retention_bounds_drop_the_rows_that_parsing_every_entry_drops() {
        let build = || {
            let conn = database();
            for (owner, rows) in [("owner", 2_600), ("other", 40)] {
                for seq in 1..=rows {
                    let key = Key {
                        carrier: "carrier".into(),
                        call_key: format!("{owner}-{seq}"),
                    };
                    // Expired entries interleave with unexpired results,
                    // results reduced to their outcome and calls that never
                    // started; the last two are error frames like expired ones.
                    let (kind, frame) = match seq % 13 {
                        0..=5 => ("expired", not_retained(&key, "ok")),
                        6 | 7 => ("not_started", not_started(&key, "denied")),
                        8 => ("result", not_retained(&key, "ok")),
                        _ => ("result", success()),
                    };
                    let entry = json!({"kind": kind, "owner": owner, "ref": "same-ref",
                        "scope_epoch": 7, "custodian": owner, "call_key": key.call_key,
                        "event_id": key.call_key, "settled_at": 100});
                    conn.execute(
                        "INSERT INTO call_ledger(carrier,call_key,digest,state,owner,scope_ref,
                            scope_epoch,custodian,frame_type,frame_body,outcome,event_id,
                            settled_at,seq,late_entry)
                         VALUES (?1,?2,'digest','Settled',?3,'same-ref',7,?3,?4,?5,'ok',?2,100,?6,?7)",
                        params![
                            key.carrier,
                            key.call_key,
                            owner,
                            frame.type_name(),
                            frame.body,
                            seq,
                            entry.to_string()
                        ],
                    )
                    .unwrap();
                }
            }
            // A running call has no late entry and counts against neither bound.
            admit(
                &conn,
                &key("running"),
                "digest",
                Some(&scope("owner")),
                State::DispatchStarted,
            )
            .unwrap();
            conn
        };
        let remaining = |conn: &Connection| -> Vec<(String, String)> {
            let mut stmt = conn
                .prepare("SELECT carrier,call_key FROM call_ledger ORDER BY rowid")
                .unwrap();
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap();
            rows.collect::<rusqlite::Result<_>>().unwrap()
        };
        let expected = build();
        drop_over_bounds_by_parsing_every_entry(&expected, "owner");
        let actual = build();
        enforce_retention_bounds(&actual, "owner").unwrap();
        assert_eq!(remaining(&actual), remaining(&expected));
        // Both bounds were exceeded, so the comparison covers each of them.
        let kept: Vec<(bool, usize)> = {
            let mut stmt = actual
                .prepare(
                    "SELECT json_extract(late_entry,'$.kind')='expired', count(*) FROM call_ledger
                     WHERE custodian='owner' AND late_entry IS NOT NULL GROUP BY 1 ORDER BY 1",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap();
            rows.collect::<rusqlite::Result<_>>().unwrap()
        };
        assert_eq!(
            kept,
            [(false, UNACKED_RESULTS_MAX), (true, EXPIRED_ENTRIES_MAX)]
        );
        assert!(get(&actual, &key("running")).unwrap().is_some());
    }

    /// Wall-clock settle time on ledgers of 20 KB results, printed for a
    /// manual before-and-after comparison. Run it with
    /// `cargo test -p agent-file-tools --lib -- --ignored --nocapture settle_time_on_large_ledgers`.
    #[test]
    #[ignore = "manual settle-time measurement"]
    fn settle_time_on_large_ledgers() {
        // Plain padding, and source-like text whose stored JSON has escapes
        // as tool output does.
        let line = "    let value = map.get(\"key\").unwrap_or(&default);\t// \u{2192} cached\n";
        let mut escaped = String::new();
        while serde_json::to_string(&escaped).unwrap().len() < 20_000 {
            escaped.push_str(line);
        }
        let dir = tempfile::tempdir().unwrap();
        for (kind, text) in [("padded", "x".repeat(20_000)), ("escaped", escaped)] {
            for (results, expired) in [(1_000, 0), (1_000, 1_000), (2_000, 0)] {
                settle_timing(dir.path(), kind, &text, results, expired);
            }
        }
    }

    /// Times 20 settles after a first one on a ledger of `results` results
    /// carrying `text` and `expired` expired entries, and prints the result.
    fn settle_timing(
        dir: &std::path::Path,
        kind: &str,
        text: &str,
        results: usize,
        expired: usize,
    ) {
        let path = dir.join(format!("bench-{kind}-{results}-{expired}.db"));
        eprintln!("building {}", path.display());
        build_ledger_file(&path, results, expired, text);
        let conn = crate::db::open(&path).unwrap();
        let entry_bytes: i64 = conn
            .query_row(
                "SELECT max(octet_length(late_entry)) FROM call_ledger WHERE custodian='owner'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut times = Vec::new();
        for i in 0..21 {
            let next = Key {
                carrier: "carrier".into(),
                call_key: format!("next-{i}"),
            };
            admit(
                &conn,
                &next,
                "digest",
                Some(&scope("owner")),
                State::DispatchStarted,
            )
            .unwrap();
            let started = std::time::Instant::now();
            settle(&conn, &next, &text_result(text), "ok", None, 100).unwrap();
            times.push(started.elapsed());
        }
        let first = times.remove(0);
        times.sort();
        eprintln!(
                "{kind}: settle on {results} results ({entry_bytes}-byte late entries) + {expired} expired: \
                 first {first:?}, then median {:?}, max {:?} over {} settles",
                times[times.len() / 2],
                times[times.len() - 1],
                times.len()
            );
    }
}
