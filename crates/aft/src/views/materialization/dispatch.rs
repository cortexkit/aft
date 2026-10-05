//! Persist ruled method edges and unknown-receiver liveness in the same
//! transaction as ordinary bindings. The input is immutable parse blobs only.
use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::{params, OptionalExtension, Transaction};

use crate::callgraph_store::join::{
    dispatch::{MemoizedResolver, Resolver},
    CallgraphBlob, ManifestBlobReader,
};
use crate::callgraph_store::{CallGraphStoreError, Result};
use crate::views::{Manifest, ManifestEntry};

pub(super) fn emit(
    transaction: &Transaction<'_>,
    manifest: &Manifest,
    reader: &impl ManifestBlobReader,
) -> Result<()> {
    let mut decoded = BTreeMap::<String, Arc<CallgraphBlob>>::new();
    for (path, entry) in manifest.entries() {
        let ManifestEntry::Regular { planes, .. } = entry else {
            continue;
        };
        let Some(key) = &planes.callgraph else {
            continue;
        };
        let Ok(path) = std::str::from_utf8(path.as_bytes()) else {
            continue;
        };
        let blob = reader
            .read_callgraph_blob_decoded(key)
            .map_err(|e| CallGraphStoreError::Unavailable(e.to_string()))?
            .ok_or_else(|| {
                CallGraphStoreError::Unavailable(format!("missing dispatch blob {key}"))
            })?;
        decoded.insert(path.to_string(), blob);
    }
    let mut resolver = Resolver::new(
        decoded
            .iter()
            .filter_map(|(file, blob)| blob.parse().map(|p| (file.clone(), p)))
            .collect(),
    );
    resolver.type_targets =
        crate::callgraph_store::join::dispatch_type_targets(manifest, reader, &resolver.files)
            .map_err(|e| CallGraphStoreError::Unavailable(e.to_string()))?;
    {
        let mut query = transaction.prepare("SELECT caller_file, module_path, target_file FROM refs WHERE kind='import' AND module_path IS NOT NULL AND target_file IS NOT NULL ORDER BY caller_file, module_path, target_file")?;
        for row in query.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (caller, module, target) = row?;
            resolver.import_targets.insert((caller, module), target);
        }
    }
    // A content-derived signature invalidates all dispatch when a receiver hint,
    // hierarchy, method set or language membership changes, not merely callers.
    let inputs = resolver
        .files
        .iter()
        .map(|(file, parse)| (file, &parse.language, &parse.dispatch, &parse.imports))
        .collect::<Vec<_>>();
    let signature = blake3::hash(
        &serde_json::to_vec(&(
            inputs,
            resolver.import_targets.iter().collect::<Vec<_>>(),
            resolver.type_targets.iter().collect::<Vec<_>>(),
        ))
        .map_err(|e| CallGraphStoreError::Unavailable(e.to_string()))?,
    )
    .to_hex()
    .to_string();
    let old: Option<String> = transaction
        .query_row(
            "SELECT v FROM meta WHERE k='view_dispatch_signature'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    transaction.execute_batch("CREATE TABLE IF NOT EXISTS view_dispatch_sites (
        caller_file TEXT NOT NULL, ordinal INTEGER NOT NULL, language TEXT NOT NULL,
        unresolved_receiver_sites INTEGER NOT NULL, dynamic_member_sites INTEGER NOT NULL,
        external INTEGER NOT NULL, PRIMARY KEY(caller_file, ordinal));
        CREATE TABLE IF NOT EXISTS view_unknown_live (
        caller_file TEXT NOT NULL, ordinal INTEGER NOT NULL, target_file TEXT NOT NULL,
        target_symbol TEXT NOT NULL, PRIMARY KEY(caller_file, ordinal, target_file, target_symbol));")?;
    // Even when hints did not change, ordinary ref emission may have replaced
    // receiver rows. Remove those guesses before re-emitting the ruled edges.
    let changed = old.as_deref() != Some(&signature);
    if changed {
        transaction.execute_batch(
            "DELETE FROM view_dispatch_sites; DELETE FROM view_unknown_live;
            DELETE FROM edges WHERE provenance IN ('exact', 'dispatch', 'name_match');",
        )?;
    }
    let memoized = MemoizedResolver::new(&resolver);
    for (file, parse) in &resolver.files {
        for site in &parse.dispatch.sites {
            let ref_id = format!("view:{file}:{}", site.ordinal);
            transaction.execute(
                "DELETE FROM edges WHERE ref_id=?1 AND provenance NOT IN ('exact', 'dispatch', 'name_match')",
                [&ref_id],
            )?;
            let resolution = memoized.resolve(file, site);
            transaction.execute("UPDATE refs SET status=?2, target_node=NULL, target_file=NULL, target_symbol=NULL WHERE ref_id=?1", params![ref_id, if resolution.targets.is_empty() { "unresolved" } else { "resolved" }])?;
            if changed {
                transaction.execute(
                    "INSERT INTO view_dispatch_sites VALUES (?1,?2,?3,?4,?5,?6)",
                    params![
                        file,
                        site.ordinal,
                        parse.language,
                        resolution.unresolved,
                        resolution.dynamic,
                        resolution.external
                    ],
                )?;
                for (target_file, target_symbol) in &resolution.protected {
                    transaction.execute(
                        "INSERT INTO view_unknown_live VALUES (?1,?2,?3,?4)",
                        params![file, site.ordinal, target_file, target_symbol],
                    )?;
                }
            }
            let caller: Option<String> = site
                .caller
                .as_ref()
                .map(|symbol| node(transaction, file, symbol))
                .transpose()?
                .flatten();
            let Some(caller) = caller else {
                continue;
            };
            for target in &resolution.targets {
                let target_node = node(transaction, &target.file, &target.symbol)?;
                transaction.execute("INSERT OR REPLACE INTO edges (edge_id, ref_id, source_node, target_node, target_file, target_symbol, kind, line, provenance)
                    VALUES (?1,?2,?3,?4,?5,?6,'call',?7,?8)", params![format!("dispatch:{file}:{}:{}:{}", site.ordinal, target.file, target.symbol), ref_id, caller, target_node, target.file, target.symbol, site.line, target.provenance])?;
            }
        }
    }
    transaction.execute(
        "INSERT OR REPLACE INTO meta(k,v) VALUES ('view_dispatch_signature',?1)",
        [signature],
    )?;
    Ok(())
}

fn node(transaction: &Transaction<'_>, file: &str, symbol: &str) -> Result<Option<String>> {
    transaction
        .query_row(
            "SELECT id FROM nodes WHERE file_path=?1 AND scoped_name=?2",
            params![file, symbol],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
}

/// Persisted counts include explicit zero rows for languages with no syntactic
/// dynamic member access. Reflection does not change these counts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SiteCounts {
    pub unresolved_receiver_sites: usize,
    pub dynamic_member_sites: usize,
    pub external: usize,
}

pub fn counts(connection: &rusqlite::Connection) -> rusqlite::Result<BTreeMap<String, SiteCounts>> {
    let mut result = [
        "python",
        "javascript",
        "typescript",
        "rust",
        "go",
        "java",
        "csharp",
        "kotlin",
    ]
    .into_iter()
    .map(|l| (l.to_string(), SiteCounts::default()))
    .collect::<BTreeMap<_, _>>();
    let mut query = connection.prepare("SELECT language, SUM(unresolved_receiver_sites), SUM(dynamic_member_sites), SUM(external) FROM view_dispatch_sites GROUP BY language")?;
    for row in query.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            SiteCounts {
                unresolved_receiver_sites: r.get(1)?,
                dynamic_member_sites: r.get(2)?,
                external: r.get(3)?,
            },
        ))
    })? {
        let (language, counts) = row?;
        result.insert(language, counts);
    }
    Ok(result)
}
