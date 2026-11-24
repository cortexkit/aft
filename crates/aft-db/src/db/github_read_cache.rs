//! Durable cache rows for rendered GitHub issues and pull requests.
//!
//! The cache belongs in AFT's existing `aft.db`. Keeping the effective
//! authentication identity as a cryptographic hash prevents cache rows from
//! exposing credentials while still keeping principals strictly isolated.

use rusqlite::{params, Connection, OptionalExtension};

const CREATE_GITHUB_READ_CACHE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS github_read_cache (
    resource_kind            TEXT NOT NULL CHECK (resource_kind IN ('issue', 'pr')),
    repository               TEXT NOT NULL,
    resource_number          INTEGER NOT NULL CHECK (resource_number > 0),
    authentication_identity_hash BLOB NOT NULL,
    canonical_text           TEXT NOT NULL,
    fetched_at_ms            INTEGER NOT NULL,
    updated_at_ms            INTEGER NOT NULL,
    PRIMARY KEY (resource_kind, repository, resource_number, authentication_identity_hash)
);
CREATE INDEX IF NOT EXISTS idx_github_read_cache_hard_ttl
    ON github_read_cache (fetched_at_ms);
"#;

/// The GitHub resource type that selects a cache namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GithubReadResourceKind {
    Issue,
    PullRequest,
}

impl GithubReadResourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pr",
        }
    }
}

/// Exact durable cache key for one GitHub resource and authentication identity.
///
/// The identity hash intentionally has no accessor or `Debug` implementation,
/// so callers can use the key without exposing an internal security boundary.
#[derive(Clone, Eq, PartialEq)]
pub struct GithubReadCacheKey {
    resource_kind: GithubReadResourceKind,
    normalized_repository: String,
    resource_number: i64,
    authentication_identity_hash: [u8; 32],
}

impl GithubReadCacheKey {
    /// Build a cache key from the repository resolved by `gh` and its effective
    /// authentication identity. Repository names are normalized case-insensitively.
    pub fn new(
        resource_kind: GithubReadResourceKind,
        resolved_repository: &str,
        resource_number: i64,
        effective_authentication_identity: &str,
    ) -> Self {
        Self {
            resource_kind,
            normalized_repository: normalize_repository(resolved_repository),
            resource_number,
            authentication_identity_hash: authentication_identity_hash(
                effective_authentication_identity,
            ),
        }
    }
}

/// Cached canonical text and the timestamps used for cache freshness decisions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubReadCacheEntry {
    pub canonical_text: String,
    /// Milliseconds since the Unix epoch when GitHub supplied the cached content.
    pub fetched_at_ms: i64,
    /// Milliseconds since the Unix epoch when this durable row was last written.
    pub updated_at_ms: i64,
}

/// Create the GitHub-read cache table and index in the already-open AFT database.
///
/// This stays separate from AFT's historical schema migrations so the cache
/// module can be registered without coupling unrelated database consumers to its
/// rollout. Every public cache operation calls this function before accessing the
/// table.
pub fn ensure_github_read_cache_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(CREATE_GITHUB_READ_CACHE_SCHEMA)
}

/// Look up one cache row using the full resource and authentication-identity key.
pub fn lookup_github_read_cache_entry(
    conn: &Connection,
    key: &GithubReadCacheKey,
) -> rusqlite::Result<Option<GithubReadCacheEntry>> {
    ensure_github_read_cache_schema(conn)?;
    lookup_github_read_cache_entry_after_schema(conn, key)
}

/// [`lookup_github_read_cache_entry`] for a caller that already ran
/// [`ensure_github_read_cache_schema`] on this connection, so a hot read path
/// does not repeat the schema statements on every call.
pub fn lookup_github_read_cache_entry_after_schema(
    conn: &Connection,
    key: &GithubReadCacheKey,
) -> rusqlite::Result<Option<GithubReadCacheEntry>> {
    conn.query_row(
        "SELECT canonical_text, fetched_at_ms, updated_at_ms
         FROM github_read_cache
         WHERE resource_kind = ?1
           AND repository = ?2
           AND resource_number = ?3
           AND authentication_identity_hash = ?4",
        params![
            key.resource_kind.as_str(),
            &key.normalized_repository,
            key.resource_number,
            key.authentication_identity_hash.as_slice(),
        ],
        |row| {
            Ok(GithubReadCacheEntry {
                canonical_text: row.get(0)?,
                fetched_at_ms: row.get(1)?,
                updated_at_ms: row.get(2)?,
            })
        },
    )
    .optional()
}

/// Insert or replace the canonical render for one exact cache key.
///
/// `fetched_at_ms` records the source-fetch time, so callers can apply fresh,
/// soft-TTL, and hard-TTL policies without deriving age from filesystem metadata.
pub fn upsert_github_read_cache_entry(
    conn: &Connection,
    key: &GithubReadCacheKey,
    canonical_text: &str,
    fetched_at_ms: i64,
) -> rusqlite::Result<()> {
    ensure_github_read_cache_schema(conn)?;
    upsert_github_read_cache_entry_after_schema(conn, key, canonical_text, fetched_at_ms)
}

/// [`upsert_github_read_cache_entry`] for a caller that already ran
/// [`ensure_github_read_cache_schema`] on this connection.
pub fn upsert_github_read_cache_entry_after_schema(
    conn: &Connection,
    key: &GithubReadCacheKey,
    canonical_text: &str,
    fetched_at_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO github_read_cache (
            resource_kind,
            repository,
            resource_number,
            authentication_identity_hash,
            canonical_text,
            fetched_at_ms,
            updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(resource_kind, repository, resource_number, authentication_identity_hash)
         DO UPDATE SET
            canonical_text = excluded.canonical_text,
            fetched_at_ms = excluded.fetched_at_ms,
            updated_at_ms = excluded.updated_at_ms",
        params![
            key.resource_kind.as_str(),
            &key.normalized_repository,
            key.resource_number,
            key.authentication_identity_hash.as_slice(),
            canonical_text,
            fetched_at_ms,
            fetched_at_ms,
        ],
    )?;
    Ok(())
}

/// Cache rows whose GitHub copy is older than this are deleted by storage
/// maintenance. A row only stands in, labelled with its age, when a live fetch
/// fails; rows are keyed per reading session, so an old one is rarely asked
/// for again.
pub const GITHUB_READ_CACHE_HARD_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Most cache rows kept; maintenance deletes the oldest beyond this.
pub const GITHUB_READ_CACHE_MAX_ROWS: i64 = 512;
/// Most rows one maintenance pass deletes, so it holds the shared connection
/// only briefly; later passes continue.
const SWEEP_LIMIT: i64 = 256;

/// One bounded retention pass: delete rows past the hard TTL, then the oldest
/// rows beyond [`GITHUB_READ_CACHE_MAX_ROWS`], at most [`SWEEP_LIMIT`] in
/// all. Both selections walk the `fetched_at_ms` index.
pub(crate) fn sweep(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    if conn.is_readonly(rusqlite::DatabaseName::Main)? {
        return Ok(0);
    }
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='github_read_cache')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(0);
    }
    let expired = conn.execute(
        "DELETE FROM github_read_cache WHERE rowid IN (
             SELECT rowid FROM github_read_cache WHERE fetched_at_ms <= ?1
             ORDER BY fetched_at_ms LIMIT ?2)",
        params![
            now_ms.saturating_sub(GITHUB_READ_CACHE_HARD_TTL_MS),
            SWEEP_LIMIT
        ],
    )?;
    let budget = SWEEP_LIMIT.saturating_sub(expired as i64);
    let over_cap = if budget > 0 {
        conn.execute(
            "DELETE FROM github_read_cache WHERE rowid IN (
                 SELECT rowid FROM github_read_cache
                 ORDER BY fetched_at_ms DESC LIMIT ?2 OFFSET ?1)",
            params![GITHUB_READ_CACHE_MAX_ROWS, budget],
        )?
    } else {
        0
    };
    if expired + over_cap > 0 {
        log::info!("GitHub read cache retention: expired={expired} over_cap={over_cap}");
    }
    Ok(expired + over_cap)
}

/// Delete rows whose source-fetch time has reached the hard-TTL cutoff.
///
/// A timestamp equal to `hard_ttl_cutoff_ms` is expired, matching the normal
/// `age >= hard_ttl` boundary used by cache callers.
pub fn evict_hard_expired_github_read_cache_entries(
    conn: &Connection,
    hard_ttl_cutoff_ms: i64,
) -> rusqlite::Result<usize> {
    ensure_github_read_cache_schema(conn)?;
    conn.execute(
        "DELETE FROM github_read_cache WHERE fetched_at_ms <= ?1",
        [hard_ttl_cutoff_ms],
    )
}

/// Invalidate a resource across identities, or only one identity when supplied.
///
/// A successful mutation can conservatively omit `effective_authentication_identity`
/// when its result may affect caches visible to more than one principal.
pub fn invalidate_github_read_cache_resource(
    conn: &Connection,
    resource_kind: GithubReadResourceKind,
    resolved_repository: &str,
    resource_number: i64,
    effective_authentication_identity: Option<&str>,
) -> rusqlite::Result<usize> {
    ensure_github_read_cache_schema(conn)?;
    invalidate_github_read_cache_resource_after_schema(
        conn,
        resource_kind,
        resolved_repository,
        resource_number,
        effective_authentication_identity,
    )
}

/// [`invalidate_github_read_cache_resource`] for a caller that already ran
/// [`ensure_github_read_cache_schema`] on this connection.
pub fn invalidate_github_read_cache_resource_after_schema(
    conn: &Connection,
    resource_kind: GithubReadResourceKind,
    resolved_repository: &str,
    resource_number: i64,
    effective_authentication_identity: Option<&str>,
) -> rusqlite::Result<usize> {
    let normalized_repository = normalize_repository(resolved_repository);

    match effective_authentication_identity {
        Some(identity) => conn.execute(
            "DELETE FROM github_read_cache
             WHERE resource_kind = ?1
               AND repository = ?2
               AND resource_number = ?3
               AND authentication_identity_hash = ?4",
            params![
                resource_kind.as_str(),
                normalized_repository,
                resource_number,
                authentication_identity_hash(identity).as_slice(),
            ],
        ),
        None => conn.execute(
            "DELETE FROM github_read_cache
             WHERE resource_kind = ?1 AND repository = ?2 AND resource_number = ?3",
            params![
                resource_kind.as_str(),
                normalized_repository,
                resource_number
            ],
        ),
    }
}

fn normalize_repository(repository: &str) -> String {
    repository.trim().to_ascii_lowercase()
}

fn authentication_identity_hash(identity: &str) -> [u8; 32] {
    *blake3::hash(identity.as_bytes()).as_bytes()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    const NOW_MS: i64 = 400 * DAY_MS;

    fn insert(conn: &Connection, number: i64, session: &str, fetched_at_ms: i64) {
        let key = GithubReadCacheKey::new(
            GithubReadResourceKind::PullRequest,
            "owner/repo",
            number,
            session,
        );
        upsert_github_read_cache_entry(conn, &key, "# PR\n", fetched_at_ms).unwrap();
    }

    fn rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM github_read_cache", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[test]
    fn storage_maintenance_evicts_expired_and_excess_github_read_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        ));
        {
            let conn = db.lock().unwrap();
            // Every new session that reads a PR adds a row of its own.
            for session in 0..10 {
                insert(
                    &conn,
                    1,
                    &format!("session:{session}"),
                    NOW_MS - 30 * DAY_MS,
                );
            }
            insert(&conn, 2, "session:fresh", NOW_MS - DAY_MS);
        }

        crate::db::compression_events::run_write_chores(&db, NOW_MS);
        let conn = db.lock().unwrap();
        assert_eq!(rows(&conn), 1, "rows past the hard TTL are deleted");

        // Past the row cap, the oldest go first, a bounded number per pass.
        for number in 0..GITHUB_READ_CACHE_MAX_ROWS + 300 {
            insert(&conn, 10 + number, "session:cap", NOW_MS - DAY_MS + number);
        }
        assert_eq!(sweep(&conn, NOW_MS).unwrap(), SWEEP_LIMIT as usize);
        assert!(sweep(&conn, NOW_MS).unwrap() > 0);
        assert_eq!(rows(&conn), GITHUB_READ_CACHE_MAX_ROWS);
        let oldest_kept: i64 = conn
            .query_row(
                "SELECT MIN(fetched_at_ms) FROM github_read_cache",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(oldest_kept > NOW_MS - DAY_MS, "the newest rows are kept");
    }
}
