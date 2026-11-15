//! Machine-scoped durable resolution and strict-verification state for standing roots.
//!
//! These rows deliberately have no harness, session, or daemon identifier: a
//! daemon and a daemonless CLI must observe the same pinned path identity.

use std::fmt;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::config::IndexKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandingRootRecord {
    pub literal_path: String,
    pub resolved_target: String,
    pub resolved_git_toplevel: Option<String>,
    pub scoped_relative_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureStandingRoot {
    Created,
    Reloaded,
}

#[derive(Debug)]
pub enum StandingRootError {
    Sqlite(rusqlite::Error),
    ResolvedPathDrift {
        literal_path: String,
        field: &'static str,
        recorded: Option<String>,
        resolved: Option<String>,
    },
    MissingFreshnessRow {
        literal_path: String,
        kind: IndexKind,
    },
}

impl fmt::Display for StandingRootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "standing roots database error: {error}"),
            Self::ResolvedPathDrift {
                literal_path,
                field,
                recorded,
                resolved,
            } => write!(
                f,
                "resolved-path-drift refusal for {literal_path:?}: {field} changed from {recorded:?} to {resolved:?}"
            ),
            Self::MissingFreshnessRow { literal_path, kind } => write!(
                f,
                "no standing freshness row exists for {literal_path:?} ({})",
                kind.as_str()
            ),
        }
    }
}

impl std::error::Error for StandingRootError {}

impl From<rusqlite::Error> for StandingRootError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Return the recorded identity for one literal configuration spelling.
pub fn get_standing_root(
    conn: &Connection,
    literal_path: &str,
) -> Result<Option<StandingRootRecord>, StandingRootError> {
    conn.query_row(
        "SELECT literal_path, resolved_target, resolved_git_toplevel, scoped_relative_path
         FROM standing_roots WHERE literal_path = ?1",
        [literal_path],
        |row| {
            Ok(StandingRootRecord {
                literal_path: row.get(0)?,
                resolved_target: row.get(1)?,
                resolved_git_toplevel: row.get(2)?,
                scoped_relative_path: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Pin a new root or confirm that an unchanged literal spelling still resolves
/// to its recorded identity. A mismatch never updates the stored record.
pub fn ensure_standing_root(
    conn: &mut Connection,
    candidate: &StandingRootRecord,
    selected_kinds: &[IndexKind],
) -> Result<EnsureStandingRoot, StandingRootError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing = get_standing_root_tx(&tx, &candidate.literal_path)?;
    let outcome = match existing {
        Some(recorded) => {
            ensure_same_identity(&recorded, candidate)?;
            reconcile_freshness_rows(&tx, &candidate.literal_path, selected_kinds, false)?;
            EnsureStandingRoot::Reloaded
        }
        None => {
            tx.execute(
                "INSERT INTO standing_roots (
                    literal_path, resolved_target, resolved_git_toplevel, scoped_relative_path
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    candidate.literal_path,
                    candidate.resolved_target,
                    candidate.resolved_git_toplevel,
                    candidate.scoped_relative_path,
                ],
            )?;
            reconcile_freshness_rows(&tx, &candidate.literal_path, selected_kinds, true)?;
            EnsureStandingRoot::Created
        }
    };
    tx.commit()?;
    Ok(outcome)
}

/// Remove the identity and every per-kind freshness row for a deleted config
/// entry. Artifact directories remain owned by normal generation GC.
pub fn delete_standing_root(
    conn: &Connection,
    literal_path: &str,
) -> Result<(), StandingRootError> {
    conn.execute(
        "DELETE FROM standing_roots WHERE literal_path = ?1",
        [literal_path],
    )?;
    Ok(())
}

pub fn needs_strict_verify(
    conn: &Connection,
    literal_path: &str,
    kind: IndexKind,
) -> Result<Option<bool>, StandingRootError> {
    conn.query_row(
        "SELECT needs_strict_verify FROM standing_root_freshness
         WHERE literal_path = ?1 AND index_kind = ?2",
        params![literal_path, kind.as_str()],
        |row| Ok(row.get::<_, i64>(0)? != 0),
    )
    .optional()
    .map_err(Into::into)
}

/// Return a standing root's per-kind freshness state by its resolved target.
/// Daemonless artifact readers know the canonical artifact root, not the user
/// configuration spelling that keys the durable row.
pub fn needs_strict_verify_for_resolved_target(
    conn: &Connection,
    resolved_target: &str,
    kind: IndexKind,
) -> Result<Option<bool>, StandingRootError> {
    conn.query_row(
        "SELECT freshness.needs_strict_verify
         FROM standing_roots AS roots
         JOIN standing_root_freshness AS freshness
           ON freshness.literal_path = roots.literal_path
         WHERE roots.resolved_target = ?1 AND freshness.index_kind = ?2",
        params![resolved_target, kind.as_str()],
        |row| Ok(row.get::<_, i64>(0)? != 0),
    )
    .optional()
    .map_err(Into::into)
}

/// Record an observation gap. A later strict verification is the only API that
/// clears this flag.
pub fn mark_needs_strict_verify(
    conn: &Connection,
    literal_path: &str,
    kind: IndexKind,
) -> Result<(), StandingRootError> {
    let updated = conn.execute(
        "UPDATE standing_root_freshness
         SET needs_strict_verify = 1, strict_verified_at = NULL
         WHERE literal_path = ?1 AND index_kind = ?2",
        params![literal_path, kind.as_str()],
    )?;
    if updated == 1 {
        Ok(())
    } else {
        Err(StandingRootError::MissingFreshnessRow {
            literal_path: literal_path.to_string(),
            kind,
        })
    }
}

/// Atomically persist a successful strict verification and clear its freshness
/// flag. Failed or interrupted verification must not call this operation.
pub fn record_successful_strict_verification(
    conn: &mut Connection,
    literal_path: &str,
    kind: IndexKind,
    verified_at_ms: i64,
) -> Result<(), StandingRootError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    record_successful_strict_verification_in_transaction(&tx, literal_path, kind, verified_at_ms)?;
    tx.commit()?;
    Ok(())
}

/// Transactional half of [`record_successful_strict_verification`]. It is
/// public for callers that also persist their successful verification outcome;
/// dropping the transaction before commit leaves `needs_strict_verify` set.
pub fn record_successful_strict_verification_in_transaction(
    tx: &Transaction<'_>,
    literal_path: &str,
    kind: IndexKind,
    verified_at_ms: i64,
) -> Result<(), StandingRootError> {
    let updated = tx.execute(
        "UPDATE standing_root_freshness
         SET strict_verified_at = ?3, needs_strict_verify = 0
         WHERE literal_path = ?1 AND index_kind = ?2",
        params![literal_path, kind.as_str(), verified_at_ms],
    )?;
    if updated == 1 {
        Ok(())
    } else {
        Err(StandingRootError::MissingFreshnessRow {
            literal_path: literal_path.to_string(),
            kind,
        })
    }
}

fn get_standing_root_tx(
    tx: &Transaction<'_>,
    literal_path: &str,
) -> Result<Option<StandingRootRecord>, StandingRootError> {
    tx.query_row(
        "SELECT literal_path, resolved_target, resolved_git_toplevel, scoped_relative_path
         FROM standing_roots WHERE literal_path = ?1",
        [literal_path],
        |row| {
            Ok(StandingRootRecord {
                literal_path: row.get(0)?,
                resolved_target: row.get(1)?,
                resolved_git_toplevel: row.get(2)?,
                scoped_relative_path: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn ensure_same_identity(
    recorded: &StandingRootRecord,
    candidate: &StandingRootRecord,
) -> Result<(), StandingRootError> {
    for (field, recorded_value, candidate_value) in [
        (
            "resolved_target",
            Some(recorded.resolved_target.clone()),
            Some(candidate.resolved_target.clone()),
        ),
        (
            "resolved_git_toplevel",
            recorded.resolved_git_toplevel.clone(),
            candidate.resolved_git_toplevel.clone(),
        ),
        (
            "scoped_relative_path",
            recorded.scoped_relative_path.clone(),
            candidate.scoped_relative_path.clone(),
        ),
    ] {
        if recorded_value != candidate_value {
            return Err(StandingRootError::ResolvedPathDrift {
                literal_path: candidate.literal_path.clone(),
                field,
                recorded: recorded_value,
                resolved: candidate_value,
            });
        }
    }
    Ok(())
}

fn reconcile_freshness_rows(
    tx: &Transaction<'_>,
    literal_path: &str,
    selected_kinds: &[IndexKind],
    creating: bool,
) -> Result<(), StandingRootError> {
    for kind in IndexKind::ALL {
        if selected_kinds.contains(&kind) {
            tx.execute(
                "INSERT INTO standing_root_freshness (
                    literal_path, index_kind, needs_strict_verify, strict_verified_at
                 ) VALUES (?1, ?2, 1, NULL)
                 ON CONFLICT(literal_path, index_kind) DO NOTHING",
                params![literal_path, kind.as_str()],
            )?;
        } else if !creating {
            tx.execute(
                "DELETE FROM standing_root_freshness
                 WHERE literal_path = ?1 AND index_kind = ?2",
                params![literal_path, kind.as_str()],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use tempfile::tempdir;

    fn record(path: &str, target: &str) -> StandingRootRecord {
        StandingRootRecord {
            literal_path: path.to_string(),
            resolved_target: target.to_string(),
            resolved_git_toplevel: Some("/repo".to_string()),
            scoped_relative_path: Some("src".to_string()),
        }
    }

    #[test]
    fn root_identity_is_machine_scoped_and_pins_literal_spelling() {
        let dir = tempdir().unwrap();
        let mut conn = db::open(&dir.path().join("aft.db")).unwrap();
        let first = record("~/work/src", "/real/repo/src");
        assert_eq!(
            ensure_standing_root(&mut conn, &first, &[IndexKind::Search, IndexKind::Semantic])
                .unwrap(),
            EnsureStandingRoot::Created
        );
        assert!(
            needs_strict_verify(&conn, "~/work/src", IndexKind::Semantic)
                .unwrap()
                .unwrap()
        );
        assert!(needs_strict_verify(&conn, "~/work/src", IndexKind::Search)
            .unwrap()
            .unwrap());

        assert_eq!(
            ensure_standing_root(&mut conn, &first, &[IndexKind::Search, IndexKind::Semantic])
                .unwrap(),
            EnsureStandingRoot::Reloaded
        );
        let drift = ensure_standing_root(
            &mut conn,
            &record("~/work/src", "/retargeted/repo/src"),
            &[IndexKind::Search],
        )
        .unwrap_err();
        assert!(matches!(
            drift,
            StandingRootError::ResolvedPathDrift {
                field: "resolved_target",
                ..
            }
        ));
        assert_eq!(
            get_standing_root(&conn, "~/work/src")
                .unwrap()
                .unwrap()
                .resolved_target,
            "/real/repo/src"
        );
    }

    #[test]
    fn deletion_removes_resolution_and_freshness_without_artifact_gc() {
        let dir = tempdir().unwrap();
        let mut conn = db::open(&dir.path().join("aft.db")).unwrap();
        ensure_standing_root(
            &mut conn,
            &record("/one", "/repo/one"),
            &[IndexKind::Search],
        )
        .unwrap();
        ensure_standing_root(
            &mut conn,
            &record("/two", "/repo/two"),
            &[IndexKind::Search],
        )
        .unwrap();
        delete_standing_root(&conn, "/one").unwrap();
        assert!(get_standing_root(&conn, "/one").unwrap().is_none());
        assert!(needs_strict_verify(&conn, "/one", IndexKind::Search)
            .unwrap()
            .is_none());
        assert!(get_standing_root(&conn, "/two").unwrap().is_some());
    }

    #[test]
    fn strict_verification_clear_is_atomic_and_drop_before_commit_keeps_flag() {
        let dir = tempdir().unwrap();
        let mut conn = db::open(&dir.path().join("aft.db")).unwrap();
        ensure_standing_root(
            &mut conn,
            &record("/one", "/repo/one"),
            &[IndexKind::Search],
        )
        .unwrap();

        {
            let tx = conn.transaction().unwrap();
            record_successful_strict_verification_in_transaction(&tx, "/one", IndexKind::Search, 9)
                .unwrap();
            // Simulate a crash after successful verification but before commit.
        }
        assert!(needs_strict_verify(&conn, "/one", IndexKind::Search)
            .unwrap()
            .unwrap());

        record_successful_strict_verification(&mut conn, "/one", IndexKind::Search, 10).unwrap();
        assert!(!needs_strict_verify(&conn, "/one", IndexKind::Search)
            .unwrap()
            .unwrap());
    }
}
