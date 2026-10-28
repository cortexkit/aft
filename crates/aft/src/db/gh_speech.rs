//! Durable speech state. History/task JSON cannot recreate these associations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use crate::gh_shim_speech::model::{
    AuthorityRecord, ExecutionSeal, LaunchAuthority, PolicySnapshot,
};
use crate::gh_shim_speech::{Code, Refusal};

use super::access_grants::ScopeKey;
use super::{SqliteStore, TrackedConnection};
use crate::gh_shim_speech::scope::ValidatedEnd;

mod legacy_dispatch;
#[cfg(test)]
pub(crate) use legacy_dispatch::LegacyDispatchStatus;
pub(crate) use legacy_dispatch::MIGRATION as LEGACY_DISPATCH_MIGRATION;

pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_millis(50);

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS gh_speech_authorities (
    incarnation TEXT PRIMARY KEY NOT NULL,
    minting_run TEXT NOT NULL,
    harness TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    record_json TEXT NOT NULL,
    record_digest BLOB NOT NULL CHECK(length(record_digest)=32),
    ticket_digest BLOB UNIQUE CHECK(ticket_digest IS NULL OR length(ticket_digest)=32),
    phase TEXT NOT NULL CHECK(phase IN ('candidate','active','revoked')),
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation>=0),
    UNIQUE(harness,session_id,task_id)
);
CREATE INDEX IF NOT EXISTS gh_speech_task_identity ON gh_speech_authorities(session_id,task_id);
CREATE TABLE IF NOT EXISTS gh_speech_task_revocations (
    incarnation TEXT PRIMARY KEY NOT NULL,
    reason TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS gh_speech_scope_ends (
    owner TEXT NOT NULL,
    scope_ref TEXT NOT NULL,
    epoch TEXT NOT NULL,
    evidence_json TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY(owner,scope_ref,epoch)
);
CREATE TABLE IF NOT EXISTS gh_speech_policy_versions (
    artifact_id TEXT NOT NULL,
    version TEXT NOT NULL,
    digest BLOB NOT NULL CHECK(length(digest)=32),
    envelope BLOB NOT NULL,
    PRIMARY KEY(artifact_id,version)
);
CREATE TABLE IF NOT EXISTS gh_speech_policy_watermarks (
    artifact_id TEXT PRIMARY KEY NOT NULL,
    version TEXT NOT NULL,
    digest BLOB NOT NULL CHECK(length(digest)=32),
    generation INTEGER NOT NULL CHECK(generation>=0)
);
CREATE TABLE IF NOT EXISTS gh_speech_requests (
    request_nonce TEXT PRIMARY KEY NOT NULL,
    incarnation TEXT NOT NULL,
    payload_digest BLOB NOT NULL CHECK(length(payload_digest)=32),
    expect_json TEXT NOT NULL,
    prepared_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS gh_speech_initialization (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    initialized INTEGER NOT NULL CHECK(initialized IN (0,1))
);
INSERT OR IGNORE INTO gh_speech_initialization(singleton,initialized) VALUES(1,0);
"#;

#[derive(Clone, Debug)]
pub(crate) struct Store {
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct StoredAuthority {
    pub(crate) record: AuthorityRecord,
    pub(crate) ticket_digest: Option<Vec<u8>>,
    pub(crate) phase: String,
    pub(crate) generation: i64,
}

fn unavailable() -> Refusal {
    Refusal::new(Code::AuthorityUnavailable, "snapshot_invalid")
}

fn db_error(error: rusqlite::Error) -> Refusal {
    if super::is_busy_error(&error) {
        Refusal::new(Code::AuthorityUnavailable, "storage_busy")
    } else {
        log::warn!("speech authority database operation failed: {error}");
        unavailable()
    }
}

fn encoded(record: &AuthorityRecord) -> Result<(String, Vec<u8>), Refusal> {
    let text = serde_json::to_string(record).map_err(|_| unavailable())?;
    let hash = blake3::hash(text.as_bytes()).as_bytes().to_vec();
    Ok((text, hash))
}

fn load(conn: &rusqlite::Connection, incarnation: &str) -> Result<StoredAuthority, Refusal> {
    let row: Option<(String, Vec<u8>, Option<Vec<u8>>, String, i64, String, String, String, String)> = conn
        .query_row(
            "SELECT record_json,record_digest,ticket_digest,phase,generation,minting_run,harness,session_id,task_id
             FROM gh_speech_authorities WHERE incarnation=?1",
            [incarnation],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?,row.get(8)?)),
        ).optional().map_err(db_error)?;
    let Some((text, digest, ticket_digest, phase, generation, minting_run, harness, session, task)) =
        row
    else {
        return Err(unavailable());
    };
    if digest.as_slice() != blake3::hash(text.as_bytes()).as_bytes() {
        return Err(unavailable());
    }
    let record: AuthorityRecord = serde_json::from_str(&text).map_err(|_| unavailable())?;
    record.launch.validate()?;
    if record.incarnation != incarnation
        || record.minting_run != minting_run
        || record.harness != harness
        || record.session_id != session
        || record.task_id != task
    {
        return Err(unavailable());
    }
    Ok(StoredAuthority {
        record,
        ticket_digest,
        phase,
        generation,
    })
}

fn check_not_revoked(conn: &rusqlite::Connection, incarnation: &str) -> Result<(), Refusal> {
    let revoked: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM gh_speech_task_revocations WHERE incarnation=?1)",
            [incarnation],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if revoked {
        return Err(Refusal::new(Code::AuthorityUnavailable, "revoked"));
    }
    Ok(())
}

fn check_scopes(conn: &rusqlite::Connection, launch: &LaunchAuthority) -> Result<(), Refusal> {
    for (role, scope) in launch.required_scopes() {
        let key = ScopeKey::from_stamp(scope);
        let ended: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM gh_speech_scope_ends WHERE owner=?1 AND scope_ref=?2 AND epoch=?3)",
            params![key.owner,key.scope_ref,key.epoch.to_string()], |row| row.get(0),
        ).map_err(db_error)?;
        if ended {
            return Err(Refusal::scoped(Code::ScopeEnded, "ended_previously", role));
        }
    }
    Ok(())
}

impl Store {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn connect(&self) -> Result<TrackedConnection, Refusal> {
        let conn = TrackedConnection::open_path_with_flags(
            &self.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            SqliteStore::AftDb,
        )
        .map_err(db_error)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(db_error)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(db_error)?;
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(db_error)?;
        if mode != "wal" || super::current_schema_version(&conn).map_err(db_error)? < 22 {
            return Err(unavailable());
        }
        Ok(conn)
    }

    pub(crate) fn capture(&self, record: &AuthorityRecord) -> Result<(), Refusal> {
        record.launch.validate()?;
        if record.incarnation.is_empty()
            || record.minting_run.is_empty()
            || record.task_id.is_empty()
            || record.session_id.is_empty()
            || record.seal.is_some()
        {
            return Err(unavailable());
        }
        let (text, digest) = encoded(record)?;
        let mut conn = self.connect()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        check_scopes(&tx, &record.launch)?;
        check_not_revoked(&tx, &record.incarnation)?;
        tx.execute(
            "INSERT INTO gh_speech_authorities(incarnation,minting_run,harness,session_id,task_id,record_json,record_digest,phase)
             VALUES(?1,?2,?3,?4,?5,?6,?7,'candidate')",
            params![record.incarnation,record.minting_run,record.harness,record.session_id,record.task_id,text,digest],
        ).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }

    pub(crate) fn seal(&self, incarnation: &str, seal: &ExecutionSeal) -> Result<(), Refusal> {
        if seal.child_pid == 0 || seal.child_start_time_ms == 0 || seal.birth_identity.is_empty() {
            return Err(Refusal::new(
                Code::AuthorityUnavailable,
                "task_unverifiable",
            ));
        }
        let mut conn = self.connect()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        check_not_revoked(&tx, incarnation)?;
        let mut stored = load(&tx, incarnation)?;
        check_scopes(&tx, &stored.record.launch)?;
        if stored.phase != "candidate" || stored.record.seal.as_ref().is_some_and(|old| old != seal)
        {
            return Err(unavailable());
        }
        stored.record.seal = Some(seal.clone());
        let (text, digest) = encoded(&stored.record)?;
        tx.execute("UPDATE gh_speech_authorities SET record_json=?2,record_digest=?3,generation=generation+1 WHERE incarnation=?1",
            params![incarnation,text,digest]).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }

    pub(crate) fn activate(
        &self,
        incarnation: &str,
        digest: &[u8; 32],
        minting_run: &str,
    ) -> Result<(), Refusal> {
        let mut conn = self.connect()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        check_not_revoked(&tx, incarnation)?;
        let stored = load(&tx, incarnation)?;
        check_scopes(&tx, &stored.record.launch)?;
        if stored.record.minting_run != minting_run
            || stored.phase != "candidate"
            || stored.record.seal.is_none()
        {
            return Err(unavailable());
        }
        if let Some(reason) = &stored.record.ineligible_reason {
            return Err(Refusal::new(Code::AuthorityUnavailable, reason));
        }
        if stored.record.policy.is_none() || stored.record.bindings.is_empty() {
            return Err(Refusal::new(
                Code::AuthorityUnavailable,
                "start_policy_missing",
            ));
        }
        tx.execute("UPDATE gh_speech_authorities SET phase='active',ticket_digest=?2,generation=generation+1 WHERE incarnation=?1",
            params![incarnation,digest.as_slice()]).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }
}

impl Store {
    /// A digest hit identifies issued state only. Authorizing it still requires
    /// loading the saved authority and checking revocation, scopes and execution.
    pub(crate) fn recognizes_ticket_digest(&self, digest: &[u8]) -> Result<bool, Refusal> {
        self.connect()?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM gh_speech_authorities WHERE ticket_digest=?1)",
                [digest],
                |row| row.get(0),
            )
            .map_err(db_error)
    }

    pub(crate) fn get(&self, incarnation: &str) -> Result<StoredAuthority, Refusal> {
        let conn = self.connect()?;
        let value = load(&conn, incarnation)?;
        check_scopes(&conn, &value.record.launch)?;
        check_not_revoked(&conn, incarnation)?;
        Ok(value)
    }

    pub(crate) fn find_digest(
        &self,
        digest: &[u8; 32],
        current_run: &str,
    ) -> Result<StoredAuthority, Refusal> {
        let conn = self.connect()?;
        let incarnation: Option<String> = conn
            .query_row(
                "SELECT incarnation FROM gh_speech_authorities WHERE ticket_digest=?1",
                [digest.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let stored = load(&conn, &incarnation.ok_or_else(unavailable)?)?;
        check_scopes(&conn, &stored.record.launch)?;
        check_not_revoked(&conn, &stored.record.incarnation)?;
        if stored.record.minting_run == current_run
            || stored.phase != "active"
            || stored.ticket_digest.as_deref() != Some(digest.as_slice())
            || stored.record.seal.is_none()
            || stored.record.ineligible_reason.is_some()
        {
            return Err(unavailable());
        }
        Ok(stored)
    }

    pub(crate) fn incarnation_for_task(
        &self,
        session: &str,
        task: &str,
    ) -> Result<Vec<String>, Refusal> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT incarnation FROM gh_speech_authorities WHERE session_id=?1 AND task_id=?2",
            )
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![session, task], |row| row.get(0))
            .map_err(db_error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)
    }

    pub(crate) fn revoke(&self, incarnation: &str, reason: &str) -> Result<(), Refusal> {
        let mut conn = self.connect()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        tx.execute("INSERT OR IGNORE INTO gh_speech_task_revocations(incarnation,reason,recorded_at_ms) VALUES(?1,?2,?3)",
            params![incarnation,reason,super::call_ledger::now_ms()]).map_err(db_error)?;
        tx.execute("UPDATE gh_speech_authorities SET phase='revoked',generation=generation+1 WHERE incarnation=?1 AND phase!='revoked'",
            [incarnation]).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }

    pub(crate) fn record_end(&self, proof: &ValidatedEnd) -> Result<(), Refusal> {
        let key = proof.key();
        let mut conn = self.connect()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        tx.execute("INSERT OR IGNORE INTO gh_speech_scope_ends(owner,scope_ref,epoch,evidence_json,recorded_at_ms) VALUES(?1,?2,?3,?4,?5)",
            params![key.owner,key.scope_ref,key.epoch.to_string(),proof.evidence().to_string(),super::call_ledger::now_ms()]).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }

    pub(crate) fn check_launch(&self, launch: &LaunchAuthority) -> Result<(), Refusal> {
        launch.validate()?;
        let conn = self.connect()?;
        check_scopes(&conn, launch)
    }

    /// Persist a stable nonce and fingerprint before acquiring the final send gate.
    pub(crate) fn prepare_request(
        &self,
        nonce: &str,
        incarnation: &str,
        payload_digest: &[u8; 32],
        expect_json: &str,
    ) -> Result<(), Refusal> {
        if nonce.is_empty() || nonce.len() > 256 || incarnation.is_empty() {
            return Err(unavailable());
        }
        let mut conn = self.connect()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let existing: Option<(String,Vec<u8>,String)> = tx.query_row(
            "SELECT incarnation,payload_digest,expect_json FROM gh_speech_requests WHERE request_nonce=?1",
            [nonce],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional().map_err(db_error)?;
        match existing {
            Some((owner, digest, expect))
                if owner == incarnation && digest == payload_digest && expect == expect_json => {}
            Some(_) => return Err(unavailable()),
            None => {
                tx.execute("INSERT INTO gh_speech_requests(request_nonce,incarnation,payload_digest,expect_json,prepared_at_ms) VALUES(?1,?2,?3,?4,?5)",
                    params![nonce,incarnation,payload_digest.as_slice(),expect_json,super::call_ledger::now_ms()]).map_err(db_error)?;
            }
        }
        tx.commit().map_err(db_error)
    }

    /// The callback must be synchronous: production passes a pre-reserved send-queue
    /// slot, so this callback only enqueues the send and never waits for a response.
    pub(crate) fn final_gate<T>(
        &self,
        incarnation: &str,
        nonce: &str,
        expected_generation: i64,
        policy_generation: i64,
        current_policy: &PolicySnapshot,
        send: impl FnOnce() -> Result<T, Refusal>,
    ) -> Result<T, GateError> {
        self.final_gate_kind(
            incarnation,
            nonce,
            expected_generation,
            policy_generation,
            current_policy,
            false,
            send,
        )
    }

    pub(crate) fn final_foreground_gate<T>(
        &self,
        incarnation: &str,
        nonce: &str,
        expected_generation: i64,
        policy_generation: i64,
        current_policy: &PolicySnapshot,
        send: impl FnOnce() -> Result<T, Refusal>,
    ) -> Result<T, GateError> {
        self.final_gate_kind(
            incarnation,
            nonce,
            expected_generation,
            policy_generation,
            current_policy,
            true,
            send,
        )
    }

    fn final_gate_kind<T>(
        &self,
        incarnation: &str,
        nonce: &str,
        expected_generation: i64,
        policy_generation: i64,
        current_policy: &PolicySnapshot,
        allow_candidate: bool,
        send: impl FnOnce() -> Result<T, Refusal>,
    ) -> Result<T, GateError> {
        let mut conn = self.connect().map_err(GateError::BeforeSend)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)
            .map_err(GateError::BeforeSend)?;
        validate_gate(
            &tx,
            incarnation,
            nonce,
            expected_generation,
            policy_generation,
            current_policy,
            allow_candidate,
        )
        .map_err(GateError::BeforeSend)?;
        let result = send().map_err(GateError::BeforeSend)?;
        tx.commit().map_err(|error| {
            log::warn!("speech gate commit failed after enqueue: {error}");
            GateError::AfterSend
        })?;
        Ok(result)
    }
}

fn validate_gate(
    conn: &rusqlite::Connection,
    incarnation: &str,
    nonce: &str,
    expected_generation: i64,
    expected_policy_generation: i64,
    current_policy: &PolicySnapshot,
    allow_candidate: bool,
) -> Result<(), Refusal> {
    let stored = load(conn, incarnation)?;
    check_scopes(conn, &stored.record.launch)?;
    check_not_revoked(conn, incarnation)?;
    if (stored.phase != "active" && !(allow_candidate && stored.phase == "candidate"))
        || stored.generation != expected_generation
    {
        return Err(unavailable());
    }
    let prepared: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM gh_speech_requests WHERE request_nonce=?1 AND incarnation=?2)",
        params![nonce,incarnation], |row| row.get(0),
    ).map_err(db_error)?;
    if !prepared {
        return Err(unavailable());
    }
    let start = stored.record.policy.as_ref().ok_or_else(unavailable)?;
    if current_policy.artifact_id != start.artifact_id || current_policy.version < start.version {
        return Err(rollback());
    }
    let generation = policy_generation(conn, current_policy)?;
    let held: String = conn
        .query_row(
            "SELECT version FROM gh_speech_policy_watermarks WHERE artifact_id=?1",
            [&current_policy.artifact_id],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if generation != expected_policy_generation || held != current_policy.version.to_string() {
        return Err(rollback());
    }
    let start_digest: Option<Vec<u8>> = conn
        .query_row(
            "SELECT digest FROM gh_speech_policy_versions WHERE artifact_id=?1 AND version=?2",
            params![start.artifact_id, start.version.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)?;
    if start_digest.as_deref() != Some(start.digest.as_slice()) || !policy_bytes_valid(start) {
        return Err(rollback());
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum GateError {
    BeforeSend(Refusal),
    AfterSend,
}

fn rollback() -> Refusal {
    Refusal::new(Code::PolicyDenied, "manifest_rollback")
}

fn policy_bytes_valid(policy: &PolicySnapshot) -> bool {
    use sha2::Digest;
    !policy.artifact_id.is_empty()
        && policy.version > 0
        && !policy.signed_envelope.is_empty()
        && policy.digest.as_slice() == sha2::Sha256::digest(&policy.signed_envelope).as_slice()
}

fn policy_generation(conn: &rusqlite::Connection, policy: &PolicySnapshot) -> Result<i64, Refusal> {
    if !policy_bytes_valid(policy) {
        return Err(rollback());
    }
    let watermark: Option<(String,Vec<u8>,i64)> = conn.query_row(
        "SELECT version,digest,generation FROM gh_speech_policy_watermarks WHERE artifact_id=?1",
        [&policy.artifact_id], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).optional().map_err(db_error)?;
    let Some((version, digest, generation)) = watermark else {
        return Err(rollback());
    };
    let held = version.parse::<u64>().map_err(|_| rollback())?;
    if held.to_string() != version || policy.version < held {
        return Err(rollback());
    }
    let mut stmt = conn
        .prepare(
            "SELECT version,digest,envelope FROM gh_speech_policy_versions WHERE artifact_id=?1",
        )
        .map_err(db_error)?;
    let rows = stmt
        .query_map([&policy.artifact_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(db_error)?;
    let mut watermark_seen = false;
    for row in rows {
        use sha2::Digest;
        let (version, hash, bytes) = row.map_err(db_error)?;
        let value = version.parse::<u64>().map_err(|_| rollback())?;
        if value.to_string() != version
            || value > held
            || hash.as_slice() != sha2::Sha256::digest(&bytes).as_slice()
        {
            return Err(rollback());
        }
        if value == held {
            if hash != digest {
                return Err(rollback());
            }
            watermark_seen = true;
        }
        if value == policy.version && hash != policy.digest {
            return Err(rollback());
        }
    }
    if !watermark_seen {
        return Err(rollback());
    }
    Ok(generation)
}

impl Store {
    /// One-time upgrade provisioning; admission never repairs an initialized ledger.
    pub(crate) fn initialize_policy(
        &self,
        policy: &PolicySnapshot,
        strict_legacy_version: u64,
    ) -> Result<i64, Refusal> {
        if !policy_bytes_valid(policy)
            || strict_legacy_version == 0
            || policy.version < strict_legacy_version
        {
            return Err(rollback());
        }
        let mut conn = self.connect()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let initialized: Option<bool> = tx
            .query_row(
                "SELECT initialized FROM gh_speech_initialization WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        if initialized == Some(false) {
            let held: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM gh_speech_policy_versions)",
                    [],
                    |row| row.get(0),
                )
                .map_err(db_error)?;
            if held {
                return Err(rollback());
            }
            tx.execute("INSERT INTO gh_speech_policy_versions(artifact_id,version,digest,envelope) VALUES(?1,?2,?3,?4)",
                params![policy.artifact_id,policy.version.to_string(),policy.digest,policy.signed_envelope]).map_err(db_error)?;
            tx.execute("INSERT INTO gh_speech_policy_watermarks(artifact_id,version,digest,generation) VALUES(?1,?2,?3,0)",
                params![policy.artifact_id,policy.version.to_string(),policy.digest]).map_err(db_error)?;
            tx.execute(
                "UPDATE gh_speech_initialization SET initialized=1 WHERE singleton=1",
                [],
            )
            .map_err(db_error)?;
        } else if initialized != Some(true) {
            return Err(rollback());
        }
        let generation = policy_generation(&tx, policy)?;
        tx.commit().map_err(db_error)?;
        Ok(generation)
    }

    pub(crate) fn advance_policy(&self, policy: &PolicySnapshot) -> Result<i64, Refusal> {
        let mut conn = self.connect()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let old_generation = policy_generation(&tx, policy)?;
        let old: String = tx
            .query_row(
                "SELECT version FROM gh_speech_policy_watermarks WHERE artifact_id=?1",
                [&policy.artifact_id],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if old != policy.version.to_string() {
            tx.execute("INSERT INTO gh_speech_policy_versions(artifact_id,version,digest,envelope) VALUES(?1,?2,?3,?4)",
                params![policy.artifact_id,policy.version.to_string(),policy.digest,policy.signed_envelope]).map_err(db_error)?;
            tx.execute("UPDATE gh_speech_policy_watermarks SET version=?2,digest=?3,generation=generation+1 WHERE artifact_id=?1",
                params![policy.artifact_id,policy.version.to_string(),policy.digest]).map_err(db_error)?;
        }
        let generation = policy_generation(&tx, policy)?;
        if generation < old_generation {
            return Err(rollback());
        }
        tx.commit().map_err(db_error)?;
        Ok(generation)
    }
}

#[cfg(test)]
mod tests;

impl Store {
    pub(crate) fn latest_policy(&self) -> Result<Option<PolicySnapshot>, Refusal> {
        let conn = self.connect()?;
        let mut stmt=conn.prepare("SELECT p.artifact_id,p.version,p.digest,p.envelope FROM gh_speech_policy_watermarks w JOIN gh_speech_policy_versions p ON p.artifact_id=w.artifact_id AND p.version=w.version WHERE p.digest=w.digest").map_err(db_error)?;
        let mut rows = stmt.query([]).map_err(db_error)?;
        let Some(row) = rows.next().map_err(db_error)? else {
            return Ok(None);
        };
        let policy = PolicySnapshot {
            artifact_id: row.get(0).map_err(db_error)?,
            version: row
                .get::<_, String>(1)
                .map_err(db_error)?
                .parse()
                .map_err(|_| rollback())?,
            digest: row.get(2).map_err(db_error)?,
            signed_envelope: row.get(3).map_err(db_error)?,
        };
        if rows.next().map_err(db_error)?.is_some() {
            return Err(rollback());
        }
        policy_generation(&conn, &policy)?;
        Ok(Some(policy))
    }
}

impl Store {
    pub(crate) fn scope_ended(
        &self,
        scope: &subc_protocol::scope::ScopeStamp,
    ) -> Result<bool, Refusal> {
        let conn = self.connect()?;
        let key = ScopeKey::from_stamp(scope);
        conn.query_row("SELECT EXISTS(SELECT 1 FROM gh_speech_scope_ends WHERE owner=?1 AND scope_ref=?2 AND epoch=?3)",params![key.owner,key.scope_ref,key.epoch.to_string()],|row|row.get(0)).map_err(db_error)
    }
}
