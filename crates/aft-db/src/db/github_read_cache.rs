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
