//! Durable permission asks for stamped Alfonso workers, on the owning aft.db handle.
//!
//! One row per (asking scope, epoch, args_hash). The row remembers the consent
//! `call_key` its open ask was first sent under, so a retried call re-sends
//! that key and Cingulate returns the same ask instead of a second one.
//!
//! An `allow_once` answer authorizes at most one tool run: the dispatch that claims it
//! moves the asked grant into `approved` and stamps itself as `holder` in one
//! statement, so a concurrent identical retry finds the ask already claimed.
//! A call that needs several outside paths asks for them one at a time; the
//! grants already approved for the same call stay on the row until that call
//! finishes, and only the holder may use them. An unused claim released on
//! hold-budget expiry can be claimed by an identical retry; cancellation
//! instead consumes that claim.
//!
//! Rollback to schema 16 requires stopping AFT, dropping access_asks, then
//! setting schema_version.version to 16; this discards every open ask.
use crate::access_policy::AccessGrant;
use crate::db::access_grants::ScopeKey;
use rusqlite::{params, Connection, OptionalExtension};

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS access_asks (
 owner TEXT NOT NULL, scope_ref TEXT NOT NULL, epoch TEXT NOT NULL, args_hash TEXT NOT NULL,
 call_key TEXT, elicitation_id TEXT, selector TEXT NOT NULL, request TEXT, pending TEXT,
 approved TEXT NOT NULL DEFAULT '[]', holder TEXT, created_at INTEGER NOT NULL,
 PRIMARY KEY(owner,scope_ref,epoch,args_hash)
);
"#;

/// Identity of one ask: the asking scope at its epoch plus the call's args hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskKey {
    pub scope: ScopeKey,
    pub args_hash: String,
}

/// One persisted ask row.
#[derive(Clone, Debug, PartialEq)]
pub struct AskRecord {
    pub key: AskKey,
    /// Consent call key of the ask still waiting for an answer, if any.
    pub call_key: Option<String>,
    pub elicitation_id: Option<String>,
    /// The `ScopeSelector` the ask was sent under, as JSON.
    pub selector: serde_json::Value,
    /// The `consent.request` parameters of the open ask.
    pub request: Option<serde_json::Value>,
    /// The capability the open ask requests.
    pub pending: Option<AccessGrant>,
    /// Capabilities already approved once for this call and not yet used up.
    pub approved: Vec<AccessGrant>,
    /// The dispatch that claimed `approved`, as `<process incarnation>:<token>`.
    pub holder: Option<String>,
}

impl AskRecord {
    /// A holder from an earlier process can never finish its call, so after a
    /// restart its claimed grants are discarded rather than reused.
    pub fn holder_is_live(&self) -> bool {
        self.holder
            .as_deref()
            .is_some_and(|holder| holder_is_current(holder))
    }
}

/// A new holder token for one dispatch in this process.
pub fn new_holder() -> String {
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).expect("operating system random source for ask holders");
    let token: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}:{token}", crate::db::call_ledger::provider_incarnation())
}

fn holder_is_current(holder: &str) -> bool {
    holder.split_once(':').is_some_and(|(incarnation, _)| {
        incarnation == crate::db::call_ledger::provider_incarnation()
    })
}

fn decode<T: serde::de::DeserializeOwned>(json: &str) -> rusqlite::Result<T> {
    serde_json::from_str(json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn encode<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("ask fields serialize")
}

const COLUMNS: &str =
    "owner,scope_ref,epoch,args_hash,call_key,elicitation_id,selector,request,pending,approved,holder";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AskRecord> {
    let epoch: String = r.get(2)?;
    Ok(AskRecord {
        key: AskKey {
            scope: ScopeKey {
                owner: r.get(0)?,
                scope_ref: r.get(1)?,
                epoch: epoch.parse().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            },
            args_hash: r.get(3)?,
        },
        call_key: r.get(4)?,
        elicitation_id: r.get(5)?,
        selector: decode(&r.get::<_, String>(6)?)?,
        request: r
            .get::<_, Option<String>>(7)?
            .map(|json| decode(&json))
            .transpose()?,
        pending: r
            .get::<_, Option<String>>(8)?
            .map(|json| decode(&json))
            .transpose()?,
        approved: decode(&r.get::<_, String>(9)?)?,
        holder: r.get(10)?,
    })
}

pub fn get(conn: &Connection, key: &AskKey) -> rusqlite::Result<Option<AskRecord>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM access_asks WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND args_hash=?4"),
        params![key.scope.owner, key.scope.scope_ref, key.scope.epoch.to_string(), key.args_hash],
        row,
    )
    .optional()
}

/// Every ask of one scope at one epoch, for withdrawal when that scope ends.
pub fn for_scope(conn: &Connection, scope: &ScopeKey) -> rusqlite::Result<Vec<AskRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM access_asks WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 ORDER BY args_hash"
    ))?;
    let rows = stmt
        .query_map(
            params![scope.owner, scope.scope_ref, scope.epoch.to_string()],
            row,
        )?
        .collect();
    rows
}

/// Record a new open ask for `key`, keeping capabilities this call already
/// had approved. Any earlier holder's claim ends here: the call that held it
/// is the one now asking for the next path.
pub fn open(
    conn: &Connection,
    key: &AskKey,
    call_key: &str,
    selector: &serde_json::Value,
    request: &serde_json::Value,
    pending: &AccessGrant,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO access_asks(owner,scope_ref,epoch,args_hash,call_key,elicitation_id,selector,request,pending,approved,holder,created_at)
         VALUES (?1,?2,?3,?4,?5,NULL,?6,?7,?8,'[]',NULL,?9)
         ON CONFLICT(owner,scope_ref,epoch,args_hash) DO UPDATE SET call_key=excluded.call_key,
           elicitation_id=NULL, selector=excluded.selector, request=excluded.request,
           pending=excluded.pending, holder=NULL",
        params![
            key.scope.owner,
            key.scope.scope_ref,
            key.scope.epoch.to_string(),
            key.args_hash,
            call_key,
            encode(selector),
            encode(request),
            encode(pending),
            now
        ],
    )?;
    Ok(())
}

/// Remember the id Cingulate gave the open ask sent under `call_key`.
pub fn set_elicitation(
    conn: &Connection,
    key: &AskKey,
    call_key: &str,
    elicitation_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE access_asks SET elicitation_id=?6 WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND args_hash=?4 AND call_key=?5",
        params![
            key.scope.owner,
            key.scope.scope_ref,
            key.scope.epoch.to_string(),
            key.args_hash,
            call_key,
            elicitation_id
        ],
    )?;
    Ok(())
}

/// Claim a granted answer for the open ask sent under `call_key`. Exactly one
/// dispatch wins: the asked capability joins `approved`, the open ask closes,
/// and `holder` becomes the only dispatch allowed to use `approved`. Returns
/// `None` when another dispatch claimed it first or the ask is gone.
pub fn claim(
    conn: &Connection,
    key: &AskKey,
    call_key: &str,
    holder: &str,
) -> rusqlite::Result<Option<Vec<AccessGrant>>> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let Some(record) = get(&tx, key)? else {
        return Ok(None);
    };
    if record.call_key.as_deref() != Some(call_key) {
        return Ok(None);
    }
    let mut approved = record.approved;
    if let Some(grant) = record.pending {
        if !approved.contains(&grant) {
            approved.push(grant);
        }
    }
    let changed = tx.execute(
        "UPDATE access_asks SET call_key=NULL, elicitation_id=NULL, request=NULL, pending=NULL, approved=?6, holder=?7
         WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND args_hash=?4 AND call_key=?5",
        params![
            key.scope.owner,
            key.scope.scope_ref,
            key.scope.epoch.to_string(),
            key.args_hash,
            call_key,
            encode(&approved),
            holder
        ],
    )?;
    tx.commit()?;
    Ok((changed == 1).then_some(approved))
}

/// The holder's call finished without asking again: its approvals are used up.
pub fn consume(conn: &Connection, key: &AskKey, holder: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM access_asks WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND args_hash=?4 AND holder=?5 AND call_key IS NULL",
        params![
            key.scope.owner,
            key.scope.scope_ref,
            key.scope.epoch.to_string(),
            key.args_hash,
            holder
        ],
    )? == 1)
}

/// Make an unused, answered approval claimable again after a hold deadline.
/// Only the current holder can restore the original ask; approvals already
/// accumulated for this call stay on the row and `claim` deduplicates them.
pub fn release_claim(
    conn: &Connection,
    key: &AskKey,
    holder: &str,
    call_key: &str,
    elicitation_id: Option<&str>,
    request: &serde_json::Value,
    pending: &AccessGrant,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE access_asks SET call_key=?6, elicitation_id=?7, request=?8, pending=?9, holder=NULL
         WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND args_hash=?4 AND holder=?5 AND call_key IS NULL",
        params![key.scope.owner, key.scope.scope_ref, key.scope.epoch.to_string(), key.args_hash,
            holder, call_key, elicitation_id, encode(request), encode(pending)],
    )? == 1)
}

/// Drop an ask outright: it was declined, refused, withdrawn, or its claim
/// belongs to a process that no longer exists.
pub fn discard(conn: &Connection, key: &AskKey) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM access_asks WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND args_hash=?4",
        params![
            key.scope.owner,
            key.scope.scope_ref,
            key.scope.epoch.to_string(),
            key.args_hash
        ],
    )? == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access_policy::{AccessOperation, GrantKind};
    use std::path::PathBuf;

    fn key(hash: &str) -> AskKey {
        AskKey {
            scope: ScopeKey {
                owner: "owner".into(),
                scope_ref: "worker".into(),
                epoch: 3,
            },
            args_hash: hash.into(),
        }
    }

    fn grant(path: &str) -> AccessGrant {
        AccessGrant {
            path: PathBuf::from(path),
            kind: GrantKind::File,
            operation: AccessOperation::Read,
        }
    }

    #[test]
    fn access_asks_allow_once_is_claimed_by_exactly_one_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let ask = key("hash");
        let selector = serde_json::json!({"ref":"worker"});
        open(
            &conn,
            &ask,
            "k1",
            &selector,
            &serde_json::json!({}),
            &grant("/a"),
            1,
        )
        .unwrap();
        set_elicitation(&conn, &ask, "k1", "acc_1").unwrap();
        let first = new_holder();
        let second = new_holder();
        assert_eq!(
            claim(&conn, &ask, "k1", &first).unwrap(),
            Some(vec![grant("/a")])
        );
        // While claimed, the same settled answer seen again grants nothing.
        assert_eq!(claim(&conn, &ask, "k1", &second).unwrap(), None);
        let record = get(&conn, &ask).unwrap().unwrap();
        assert!(record.holder_is_live());
        assert_eq!(record.call_key, None);
        // A second outside path of the same call keeps the first approval.
        open(
            &conn,
            &ask,
            "k2",
            &selector,
            &serde_json::json!({}),
            &grant("/b"),
            2,
        )
        .unwrap();
        assert_eq!(get(&conn, &ask).unwrap().unwrap().holder, None);
        assert_eq!(
            claim(&conn, &ask, "k2", &second).unwrap(),
            Some(vec![grant("/a"), grant("/b")])
        );
        assert!(!consume(&conn, &ask, &first).unwrap());
        assert!(consume(&conn, &ask, &second).unwrap());
        assert_eq!(get(&conn, &ask).unwrap(), None);
    }

    #[test]
    fn access_asks_are_keyed_by_scope_epoch_and_args_hash() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let selector = serde_json::json!({});
        let base = key("hash");
        open(&conn, &base, "k1", &selector, &selector, &grant("/a"), 1).unwrap();
        let mut epoch = base.clone();
        epoch.scope.epoch = 4;
        let mut hash = base.clone();
        hash.args_hash = "other".into();
        assert_eq!(get(&conn, &epoch).unwrap(), None);
        assert_eq!(get(&conn, &hash).unwrap(), None);
        assert_eq!(for_scope(&conn, &base.scope).unwrap().len(), 1);
        assert!(discard(&conn, &base).unwrap());
        assert!(for_scope(&conn, &base.scope).unwrap().is_empty());
    }

    #[test]
    fn access_asks_release_unused_claim_preserves_answer_and_exclusive_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let ask = key("hash");
        let selector = serde_json::json!({"ref":"worker"});
        let request = serde_json::json!({"tool":"read"});
        open(&conn, &ask, "k1", &selector, &request, &grant("/a"), 1).unwrap();
        set_elicitation(&conn, &ask, "k1", "acc_1").unwrap();
        let original = get(&conn, &ask).unwrap().unwrap();
        let first = new_holder();
        let second = new_holder();
        let release = |holder: &str| {
            release_claim(
                &conn,
                &ask,
                holder,
                "k1",
                Some("acc_1"),
                &request,
                &grant("/a"),
            )
        };
        assert_eq!(
            claim(&conn, &ask, "k1", &first).unwrap(),
            Some(vec![grant("/a")])
        );
        assert!(!release(&second).unwrap());
        assert!(release(&first).unwrap());
        let released = get(&conn, &ask).unwrap().unwrap();
        assert_eq!(released.call_key, original.call_key);
        assert_eq!(released.elicitation_id, original.elicitation_id);
        assert_eq!(released.request, original.request);
        assert_eq!(released.pending, original.pending);
        assert_eq!(released.approved, vec![grant("/a")]);
        assert_eq!(released.holder, None);
        assert_eq!(
            claim(&conn, &ask, "k1", &second).unwrap(),
            Some(vec![grant("/a")])
        );
        assert!(!release(&first).unwrap());
        open(&conn, &ask, "k2", &selector, &request, &grant("/b"), 2).unwrap();
        assert!(!release(&second).unwrap());
        assert_eq!(
            get(&conn, &ask).unwrap().unwrap().call_key.as_deref(),
            Some("k2")
        );
        let third = new_holder();
        assert_eq!(
            claim(&conn, &ask, "k2", &third).unwrap(),
            Some(vec![grant("/a"), grant("/b")])
        );
        assert!(consume(&conn, &ask, &third).unwrap());
        assert!(!release(&third).unwrap());
    }

    #[test]
    fn access_asks_holder_from_an_earlier_process_is_not_live() {
        let record = AskRecord {
            key: key("hash"),
            call_key: None,
            elicitation_id: None,
            selector: serde_json::json!({}),
            request: None,
            pending: None,
            approved: vec![grant("/a")],
            holder: Some("earlier-process:00".into()),
        };
        assert!(!record.holder_is_live());
    }
}
