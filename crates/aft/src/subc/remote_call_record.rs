//! Audit one received frame independently of whether it claims a ledger key.

use super::*;
use crate::db::{call_ledger as ledger, remote_audit_rows as audit};

pub(super) type Db = Arc<StdMutex<crate::db::TrackedConnection>>;

#[derive(Clone)]
pub(super) struct Record {
    pub(super) db: Db,
    row: i64,
}

fn store_error(error: impl std::fmt::Display) -> ErrorBody {
    log::warn!("remote tool call store: {error}");
    admission_refusal()
}

impl Record {
    pub(super) fn open(
        app: &App,
        database: &Path,
        now: i64,
        scope: Option<&ScopeStamp>,
        tool: Option<&str>,
    ) -> Result<Self, ErrorBody> {
        let db = app.open_db(database).map_err(store_error)?;
        let facts = scope.and_then(|scope| scope.attributes.federated.as_ref());
        let row = {
            let conn = db.lock().map_err(store_error)?;
            audit::insert(
                &conn,
                now,
                facts.and_then(|facts| facts.peer_box_id.as_deref()),
                facts.map(|facts| facts.pairing_id.as_str()),
                facts.and_then(caller),
                tool,
            )
            .map_err(store_error)?
        };
        Ok(Self { db, row })
    }

    pub(super) fn resolve(&self, path: Option<&Path>, mode: Option<&str>) -> Result<(), ErrorBody> {
        let conn = self.db.lock().map_err(store_error)?;
        audit::resolve(&conn, self.row, path.and_then(Path::to_str), mode).map_err(store_error)
    }

    pub(super) fn claim(
        &self,
        key: &ledger::Key,
        digest: &str,
        scope: &ScopeStamp,
    ) -> Result<ledger::Admission, ErrorBody> {
        let conn = self.db.lock().map_err(store_error)?;
        ledger::reduce_expired(&conn, key, ledger::now_ms())
            .and_then(|_| {
                ledger::admit(
                    &conn,
                    key,
                    digest,
                    Some(&tool_provider::ledger_scope(scope)),
                    ledger::State::Prepared,
                )
            })
            .map_err(store_error)
    }

    pub(super) fn begin(&self, key: &ledger::Key, decision: &str) -> Result<(), ErrorBody> {
        self.begin_with_grant(key, decision, None)
    }

    pub(super) fn begin_with_grant(
        &self,
        key: &ledger::Key,
        decision: &str,
        grant: Option<&str>,
    ) -> Result<(), ErrorBody> {
        let conn = self.db.lock().map_err(store_error)?;
        if let Some(grant) = grant {
            if !crate::db::remote_grant_rows::exists(&conn, grant).map_err(store_error)? {
                return Err(authorization::denied());
            }
        }
        audit::mark(&conn, self.row, decision, None).map_err(store_error)?;
        ledger::transition(
            &conn,
            key,
            ledger::State::Prepared,
            ledger::State::DispatchStarted,
        )
        .and_then(require_transition)
        .map_err(store_error)
    }

    pub(super) fn hold(&self, key: &ledger::Key, body: &[u8]) -> Result<(), ErrorBody> {
        let conn = self.db.lock().map_err(store_error)?;
        ledger::hold_watch(&conn, key, body).map_err(store_error)
    }

    pub(super) fn finish(
        &self,
        key: Option<&ledger::Key>,
        reply: &ledger::RecordedFrame,
        decision: &str,
        reason: Option<&str>,
    ) -> Result<(), ErrorBody> {
        let conn = self.db.lock().map_err(store_error)?;
        let outcome = ledger::outcome(reply);
        audit::mark(&conn, self.row, decision, Some(&outcome)).map_err(store_error)?;
        if let Some(key) = key {
            ledger::settle(&conn, key, reply, &outcome, reason, ledger::now_ms())
                .and_then(require_transition)
                .map_err(store_error)?;
        }
        Ok(())
    }
}

fn require_transition(updated: bool) -> rusqlite::Result<()> {
    if updated {
        Ok(())
    } else {
        Err(rusqlite::Error::QueryReturnedNoRows)
    }
}
