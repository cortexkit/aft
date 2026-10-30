//! A family-level index of published derived call-graph databases, keyed by
//! the identity of their build output.
//!
//! A checkout without a usable derived base looks here for a donor in another
//! scope of its family: first one whose output is interchangeable with what it
//! would build, then the nearest recent one it can patch. That avoids both a
//! cold build and the `git worktree list` census, which only knows worktrees
//! of one repository and only offers the most recently published donor.
//!
//! Rows are hints, never protection. Every candidate is checked again before
//! use (its scope still serves that generation, a query pin, the reader's
//! readiness check, its manifest), and the copy is then patched by the
//! fingerprint-checked incremental diff from the donor's own manifest. A stale
//! or even wrong row can therefore cost a patch or a cold build, but cannot
//! put another manifest's graph in front of a reader.
//!
//! The file lives beside the family's v1 blob stores,
//! `<storage>/blobs/<family>/derived-index.sqlite`. Retention and GC open
//! those stores by name and do not enumerate or delete siblings.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::params;

use crate::views::{Manifest, ManifestEntry, RegularPlanes, SyntheticPlanes};

const INDEX_FILE: &str = "derived-index.sqlite";
pub(crate) const SEAM: &str = "views::assembly::derived_index";
/// Rows of each kind (exact identity, other recent outputs) a lookup checks.
/// Each check loads a donor manifest, so this bounds a seedless publication.
pub(super) const CANDIDATES_PER_KIND: usize = 8;
/// Newest scopes a family keeps rows for; older rows are trimmed at record.
const RETAINED_ROWS: usize = 4096;
const BUSY_TIMEOUT: Duration = Duration::from_secs(1);

/// Domain tag of [`identity`]. Change it when the identity's definition
/// changes, so rows written under the old definition stop matching.
const IDENTITY_DOMAIN: &[u8] = b"aft-derived-identity-v1";

/// The identity of the derived database a manifest builds in `family`.
///
/// Two databases with equal identities are interchangeable. The identity
/// covers everything the derived build reads:
///
/// - the family, whose blob store resolves the manifest's keys;
/// - the callgraph store schema and build-output version, which together are
///   the fingerprint the reader's readiness check demands;
/// - the materialization version of the cached state the patcher trusts;
/// - the manifest's path identity version, and for every entry in path order
///   its path bytes, kind and the fields the materializer reads.
///
/// Blob payloads enter through their keys: a callgraph key is
/// `(blake3(bytes), language, producer version)`, so equal keys name equal
/// parse results. Semantic keys are left out on purpose: the call-graph build
/// never reads them, matching `manifest_callgraph_equivalent`.
pub(super) fn identity(family: &str, manifest: &Manifest) -> [u8; 32] {
    identity_for(
        family,
        &crate::callgraph_store::derived_build_fingerprint(),
        crate::views::materialization::MATERIALIZATION_VERSION,
        manifest,
    )
}

fn identity_for(
    family: &str,
    build_fingerprint: &str,
    materialization_version: &str,
    manifest: &Manifest,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    // Every field is length-prefixed, so two different field sequences never
    // encode to the same bytes before hashing.
    let mut field = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(IDENTITY_DOMAIN);
    field(family.as_bytes());
    field(build_fingerprint.as_bytes());
    field(materialization_version.as_bytes());
    field(&[manifest.path_identity_version]);
    for (path, entry) in manifest.entries() {
        field(path.as_bytes());
        // Destructure every field, so a new manifest field fails to compile
        // here until someone decides whether the derived build reads it.
        match entry {
            ManifestEntry::Regular {
                mode,
                planes:
                    RegularPlanes {
                        semantic: _,
                        callgraph,
                    },
                resolution_input,
            } => {
                field(b"regular");
                field(&mode.to_le_bytes());
                field(&[u8::from(*resolution_input)]);
                match callgraph {
                    Some(key) => {
                        field(b"callgraph");
                        field(key.as_bytes());
                    }
                    None => field(b"no-callgraph"),
                }
            }
            ManifestEntry::Symlink { target_bytes } => {
                field(b"symlink");
                field(target_bytes.as_bytes());
            }
            ManifestEntry::Gitlink { oid } => {
                field(b"gitlink");
                field(oid.as_bytes());
            }
            ManifestEntry::Synthetic {
                name,
                planes: SyntheticPlanes { callgraph },
            } => {
                field(b"synthetic");
                field(name.as_bytes());
                field(callgraph.as_bytes());
            }
        }
    }
    *hasher.finalize().as_bytes()
}

/// The index of the family whose callgraph blob store is `callgraph_store`.
pub(super) fn index_path(callgraph_store: &Path) -> PathBuf {
    callgraph_store.with_file_name(INDEX_FILE)
}

/// A published derived output another scope may seed from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Candidate {
    pub scope: String,
    pub generation: String,
    /// The row's identity equals the one looked up.
    pub exact: bool,
}

fn open(path: &Path) -> rusqlite::Result<crate::db::file_identity::IdentityConnection> {
    let connection = crate::db::file_identity::IdentityConnection::open(path, SEAM)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    // A lost row only costs a missed reuse, so the index needs no FULL sync.
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS derived_outputs (
             scope TEXT PRIMARY KEY,
             identity BLOB NOT NULL,
             generation TEXT NOT NULL,
             published_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS derived_outputs_by_identity
             ON derived_outputs(identity, published_at);
         CREATE INDEX IF NOT EXISTS derived_outputs_by_time
             ON derived_outputs(published_at);",
    )?;
    Ok(connection)
}

/// Record that `scope` now serves `generation`, whose ready derived database
/// has `identity`. One row per scope: a later publication replaces it.
pub(super) fn record(
    path: &Path,
    scope: &str,
    generation: &str,
    identity: &[u8; 32],
) -> rusqlite::Result<()> {
    let mut connection = open(path)?;
    let published_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64);
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    transaction.execute(
        "INSERT INTO derived_outputs(scope, identity, generation, published_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(scope) DO UPDATE SET identity = excluded.identity,
             generation = excluded.generation, published_at = excluded.published_at",
        params![scope, &identity[..], generation, published_at],
    )?;
    transaction.execute(
        "DELETE FROM derived_outputs WHERE scope NOT IN (
             SELECT scope FROM derived_outputs
             ORDER BY published_at DESC, scope LIMIT ?1)",
        [RETAINED_ROWS as i64],
    )?;
    transaction.commit()
}

/// Rows other scopes recorded: up to [`CANDIDATES_PER_KIND`] with this exact
/// identity, newest first, then as many of the newest other outputs as
/// candidates for an incremental patch. A missing index has no rows.
pub(super) fn candidates(
    path: &Path,
    identity: &[u8; 32],
    exclude_scope: &str,
) -> rusqlite::Result<Vec<Candidate>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let connection = open(path)?;
    let mut rows = Vec::new();
    for (exact, sql) in [
        (
            true,
            "SELECT scope, generation FROM derived_outputs
             WHERE identity = ?1 AND scope <> ?2
             ORDER BY published_at DESC, scope LIMIT ?3",
        ),
        (
            false,
            "SELECT scope, generation FROM derived_outputs
             WHERE identity <> ?1 AND scope <> ?2
             ORDER BY published_at DESC, scope LIMIT ?3",
        ),
    ] {
        let mut statement = connection.prepare(sql)?;
        let found = statement.query_map(
            params![&identity[..], exclude_scope, CANDIDATES_PER_KIND as i64],
            |row| {
                Ok(Candidate {
                    scope: row.get(0)?,
                    generation: row.get(1)?,
                    exact,
                })
            },
        )?;
        for candidate in found {
            rows.push(candidate?);
        }
    }
    Ok(rows)
}

/// Drop a row whose scope no longer serves a ready database at that
/// generation. The generation condition keeps a row the scope replaced
/// meanwhile.
pub(super) fn forget(path: &Path, scope: &str, generation: &str) -> rusqlite::Result<()> {
    open(path)?.execute(
        "DELETE FROM derived_outputs WHERE scope = ?1 AND generation = ?2",
        params![scope, generation],
    )?;
    Ok(())
}

/// The identity recorded for `scope`, for tests.
#[cfg(test)]
pub(super) fn recorded_identity(path: &Path, scope: &str) -> Option<[u8; 32]> {
    use rusqlite::OptionalExtension;
    let identity: Vec<u8> = open(path)
        .ok()?
        .query_row(
            "SELECT identity FROM derived_outputs WHERE scope = ?1",
            [scope],
            |row| row.get(0),
        )
        .optional()
        .ok()??;
    identity.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::{ByteString, RelPath};

    fn regular(callgraph: &str, semantic: Option<&str>) -> ManifestEntry {
        ManifestEntry::Regular {
            mode: 0o100644,
            resolution_input: false,
            planes: RegularPlanes {
                semantic: semantic.map(str::to_owned),
                callgraph: Some(callgraph.to_owned()),
            },
        }
    }

    fn manifest(entries: Vec<(&str, ManifestEntry)>) -> Manifest {
        Manifest::new(
            entries
                .into_iter()
                .map(|(path, entry)| (RelPath::new(path.as_bytes().to_vec()).unwrap(), entry)),
        )
        .unwrap()
    }

    fn key(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn base() -> Manifest {
        manifest(vec![
            ("a.rs", regular(&key('a'), Some(&key('s')))),
            (
                "link",
                ManifestEntry::Symlink {
                    target_bytes: ByteString::new(b"a.rs".to_vec()),
                },
            ),
            (
                "sub",
                ManifestEntry::Gitlink {
                    oid: "1".repeat(40),
                },
            ),
        ])
    }

    fn id(manifest: &Manifest) -> [u8; 32] {
        identity_for("family", "build", "12", manifest)
    }

    #[test]
    fn identity_ignores_semantic_keys_only() {
        let without_semantic = manifest(vec![
            ("a.rs", regular(&key('a'), None)),
            (
                "link",
                ManifestEntry::Symlink {
                    target_bytes: ByteString::new(b"a.rs".to_vec()),
                },
            ),
            (
                "sub",
                ManifestEntry::Gitlink {
                    oid: "1".repeat(40),
                },
            ),
        ]);
        assert_eq!(id(&base()), id(&without_semantic));
        assert!(
            crate::views::materialization::manifest_callgraph_equivalent(
                &base(),
                &without_semantic
            )
        );
    }

    #[test]
    fn identity_changes_with_every_input_the_derived_build_reads() {
        let reference = id(&base());
        let with = |path: &str, entry: ManifestEntry| {
            let mut entries = base()
                .entries()
                .map(|(path, entry)| (path.clone(), entry.clone()))
                .filter(|(existing, _)| existing.as_bytes() != path.as_bytes())
                .collect::<Vec<_>>();
            let path = match &entry {
                ManifestEntry::Synthetic { name, .. } => RelPath::synthetic(name).unwrap(),
                _ => RelPath::new(path.as_bytes().to_vec()).unwrap(),
            };
            entries.push((path, entry));
            Manifest::new(entries).unwrap()
        };
        let variants = [
            (
                "payload key",
                id(&with("a.rs", regular(&key('b'), Some(&key('s'))))),
            ),
            (
                "mode",
                id(&with(
                    "a.rs",
                    ManifestEntry::Regular {
                        mode: 0o100755,
                        resolution_input: false,
                        planes: RegularPlanes {
                            semantic: Some(key('s')),
                            callgraph: Some(key('a')),
                        },
                    },
                )),
            ),
            (
                "resolution input",
                id(&with(
                    "a.rs",
                    ManifestEntry::Regular {
                        mode: 0o100644,
                        resolution_input: true,
                        planes: RegularPlanes {
                            semantic: Some(key('s')),
                            callgraph: Some(key('a')),
                        },
                    },
                )),
            ),
            (
                "missing callgraph key",
                id(&with(
                    "a.rs",
                    ManifestEntry::Regular {
                        mode: 0o100644,
                        resolution_input: false,
                        planes: RegularPlanes {
                            semantic: Some(key('s')),
                            callgraph: None,
                        },
                    },
                )),
            ),
            (
                "symlink target",
                id(&with(
                    "link",
                    ManifestEntry::Symlink {
                        target_bytes: ByteString::new(b"b.rs".to_vec()),
                    },
                )),
            ),
            (
                "gitlink oid",
                id(&with(
                    "sub",
                    ManifestEntry::Gitlink {
                        oid: "2".repeat(40),
                    },
                )),
            ),
            ("added path", id(&with("b.rs", regular(&key('a'), None)))),
            (
                "synthetic member",
                id(&with(
                    "\0go.work",
                    ManifestEntry::Synthetic {
                        name: "go.work".into(),
                        planes: SyntheticPlanes {
                            callgraph: key('c'),
                        },
                    },
                )),
            ),
            ("family", identity_for("other", "build", "12", &base())),
            (
                "build output version",
                identity_for("family", "build-next", "12", &base()),
            ),
            (
                "materialization version",
                identity_for("family", "build", "13", &base()),
            ),
        ];
        let mut seen = std::collections::BTreeSet::from([reference]);
        for (label, variant) in variants {
            assert_ne!(variant, reference, "{label} must change the identity");
            assert!(
                seen.insert(variant),
                "{label} collided with another variant"
            );
        }
    }

    #[test]
    fn identity_uses_the_current_build_output_fingerprint() {
        assert_eq!(
            identity("family", &base()),
            identity_for(
                "family",
                &crate::callgraph_store::derived_build_fingerprint(),
                crate::views::materialization::MATERIALIZATION_VERSION,
                &base()
            )
        );
        assert_ne!(
            identity("family", &base()),
            identity_for(
                "family",
                "an older build output",
                crate::views::materialization::MATERIALIZATION_VERSION,
                &base()
            )
        );
    }

    #[test]
    fn index_lists_exact_rows_before_recent_ones_and_forgets_by_generation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(INDEX_FILE);
        assert!(candidates(&path, &[1; 32], "self").unwrap().is_empty());
        assert!(!path.exists(), "a lookup must not create the index");
        record(&path, "near", "g1", &[2; 32]).unwrap();
        record(&path, "exact", "g1", &[1; 32]).unwrap();
        record(&path, "self", "g1", &[1; 32]).unwrap();
        let rows = candidates(&path, &[1; 32], "self").unwrap();
        assert_eq!(
            rows,
            vec![
                Candidate {
                    scope: "exact".into(),
                    generation: "g1".into(),
                    exact: true
                },
                Candidate {
                    scope: "near".into(),
                    generation: "g1".into(),
                    exact: false
                },
            ]
        );
        // A newer publication replaces the scope's row.
        record(&path, "exact", "g2", &[3; 32]).unwrap();
        assert_eq!(recorded_identity(&path, "exact"), Some([3; 32]));
        forget(&path, "exact", "g1").unwrap();
        assert_eq!(
            recorded_identity(&path, "exact"),
            Some([3; 32]),
            "forgetting an old generation must keep the scope's newer row"
        );
        forget(&path, "exact", "g2").unwrap();
        assert_eq!(recorded_identity(&path, "exact"), None);
    }
}
