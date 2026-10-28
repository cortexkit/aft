use super::*;

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS gh_speech_legacy_dispatch (
    request_nonce TEXT PRIMARY KEY NOT NULL,
    incarnation TEXT NOT NULL,
    forwarding_run TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('queued','settled')),
    enqueued_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS gh_speech_legacy_pending ON gh_speech_legacy_dispatch(incarnation,state);
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LegacyDispatchStatus {
    Queued,
    Settled,
}

impl Store {
    /// A persisted enqueue with no retained reply is an unknown outcome, including
    /// after a daemon restart. It is never permission to blindly enqueue it again.
    pub(crate) fn legacy_dispatch_status(
        &self,
        nonce: &str,
        incarnation: &str,
        payload_digest: &[u8; 32],
    ) -> Result<Option<LegacyDispatchStatus>, Refusal> {
        let conn = self.connect()?;
        let row: Option<(String, String, Vec<u8>)> = conn.query_row(
            "SELECT d.incarnation,d.state,r.payload_digest FROM gh_speech_legacy_dispatch d JOIN gh_speech_requests r USING(request_nonce) WHERE d.request_nonce=?1",
            [nonce], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional().map_err(db_error)?;
        let Some((owner, state, digest)) = row else {
            return Ok(None);
        };
        if owner != incarnation || digest.as_slice() != payload_digest {
            return Err(unavailable());
        }
        match state.as_str() {
            "queued" => Ok(Some(LegacyDispatchStatus::Queued)),
            "settled" => Ok(Some(LegacyDispatchStatus::Settled)),
            _ => Err(unavailable()),
        }
    }

    #[cfg(test)]
    pub(crate) fn final_legacy_gate<T>(
        &self,
        incarnation: &str,
        nonce: &str,
        expected_generation: i64,
        policy_generation: i64,
        current_policy: &PolicySnapshot,
        forwarding_run: &str,
        enqueue: impl FnOnce() -> Result<T, Refusal>,
    ) -> Result<T, GateError> {
        let mut conn = self.connect().map_err(GateError::BeforeSend)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)
            .map_err(GateError::BeforeSend)?;
        let duplicate: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM gh_speech_legacy_dispatch WHERE request_nonce=?1)",
                [nonce],
                |row| row.get(0),
            )
            .map_err(db_error)
            .map_err(GateError::BeforeSend)?;
        if duplicate {
            return Err(GateError::AfterSend);
        }
        validate_gate(
            &tx,
            incarnation,
            nonce,
            expected_generation,
            policy_generation,
            current_policy,
            false,
        )
        .map_err(GateError::BeforeSend)?;
        tx.execute("INSERT INTO gh_speech_legacy_dispatch(request_nonce,incarnation,forwarding_run,state,enqueued_at_ms) VALUES(?1,?2,?3,'queued',?4)",
            params![nonce,incarnation,forwarding_run,super::super::call_ledger::now_ms()])
            .map_err(db_error).map_err(GateError::BeforeSend)?;
        let queued = enqueue().map_err(GateError::BeforeSend)?;
        tx.commit().map_err(|_| GateError::AfterSend)?;
        Ok(queued)
    }

    #[cfg(test)]
    pub(crate) fn settle_legacy_dispatch(
        &self,
        nonce: &str,
        incarnation: &str,
        forwarding_run: &str,
    ) -> Result<(), Refusal> {
        let mut conn = self.connect()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let changed = tx.execute("UPDATE gh_speech_legacy_dispatch SET state='settled' WHERE request_nonce=?1 AND incarnation=?2 AND forwarding_run=?3 AND state='queued'",
            params![nonce,incarnation,forwarding_run]).map_err(db_error)?;
        if changed != 1 {
            return Err(unavailable());
        }
        tx.commit().map_err(db_error)
    }

    pub(crate) fn pending_legacy_for_identity(
        &self,
        session: &str,
        task: &str,
    ) -> Result<u64, Refusal> {
        let conn = self.connect()?;
        conn.query_row("SELECT COUNT(*) FROM gh_speech_legacy_dispatch d JOIN gh_speech_authorities a ON a.incarnation=d.incarnation WHERE a.session_id=?1 AND a.task_id=?2 AND d.state='queued'",
            params![session,task], |row| row.get(0)).map_err(db_error)
    }
}

impl Store {
    pub(crate) fn pending_legacy_for_scope(
        &self,
        scope: &subc_protocol::scope::ScopeStamp,
    ) -> Result<u64, Refusal> {
        let conn = self.connect()?;
        let mut query = conn.prepare("SELECT a.record_json,a.record_digest,COUNT(*) FROM gh_speech_legacy_dispatch d JOIN gh_speech_authorities a ON a.incarnation=d.incarnation WHERE d.state='queued' GROUP BY a.incarnation").map_err(db_error)?;
        let rows = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            })
            .map_err(db_error)?;
        let expected = ScopeKey::from_stamp(scope);
        let mut count = 0_u64;
        for row in rows {
            let (text, digest, pending) = row.map_err(db_error)?;
            if digest.as_slice() != blake3::hash(text.as_bytes()).as_bytes() {
                return Err(unavailable());
            }
            let record: AuthorityRecord = serde_json::from_str(&text).map_err(|_| unavailable())?;
            record.launch.validate()?;
            let lifecycle = record
                .launch
                .run
                .as_ref()
                .unwrap_or(&record.launch.authority);
            if ScopeKey::from_stamp(lifecycle) == expected {
                count = count.saturating_add(pending);
            }
        }
        Ok(count)
    }
}

impl Store {
    /// A dead/revoked ticket may report an already queued attempt, but this lookup
    /// grants no new dispatch and is never used for context or worker reads.
    pub(crate) fn legacy_replay_for_digest(
        &self,
        digest: &[u8; 32],
        nonce: &str,
        payload: &[u8; 32],
    ) -> Result<bool, Refusal> {
        let conn = self.connect()?;
        let incarnation: Option<String> = conn.query_row("SELECT a.incarnation FROM gh_speech_authorities a JOIN gh_speech_legacy_dispatch d ON d.incarnation=a.incarnation WHERE a.ticket_digest=?1 AND d.request_nonce=?2", params![digest.as_slice(),nonce], |row| row.get(0)).optional().map_err(db_error)?;
        let Some(incarnation) = incarnation else {
            return Ok(false);
        };
        let _record = load(&conn, &incarnation)?;
        Ok(self
            .legacy_dispatch_status(nonce, &incarnation, payload)?
            .is_some())
    }
}
