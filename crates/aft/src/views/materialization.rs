use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::callgraph_store::{
    initialize_schema, join, set_meta_ready, CallGraphStoreError, Result, PROVENANCE_TREESITTER,
};

/// Cold materialization, also used to upgrade databases without diff metadata.
pub fn materialize_manifest_view_database(
    database_path: &Path,
    callgraph_blob_database: &Path,
    manifest: &crate::views::Manifest,
) -> Result<()> {
    materialize(database_path, callgraph_blob_database, manifest, None).map(|_| ())
}

/// Counts affected graph and dependency-cache rows, excluding readiness and metadata.
/// Replacements count both the deletion and insertion; relinks include refs and edges.
/// Resolution counters describe work performed, not SQLite writes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MaterializeStats {
    pub deleted: usize,
    pub inserted: usize,
    pub relinked_deleted: usize,
    pub relinked_inserted: usize,
    pub dependency_deleted: usize,
    pub dependency_inserted: usize,
    pub dependent_files: usize,
    pub resolved_files: usize,
    pub resolved_refs: usize,
    pub rebuilt_surface_entries: usize,
    pub decoded_caller_blobs: usize,
    pub full_resolution: bool,
}

impl MaterializeStats {
    pub fn graph_rows_written(&self) -> usize {
        self.deleted + self.inserted + self.relinked_deleted + self.relinked_inserted
    }

    pub fn rows_written(&self) -> usize {
        self.graph_rows_written() + self.dependency_deleted + self.dependency_inserted
    }
}

/// Apply to a private copy of the base generation's database, never a published file.
/// The caller owns copying, durability and pointer publication. All graph changes and
/// the fingerprint commit atomically; errors roll back to the supplied base. An old
/// binding-cache schema takes the cold path; a mismatched manifest is refused.
pub fn apply_manifest_diff(
    database_path: &Path,
    base_manifest: &crate::views::Manifest,
    new_manifest: &crate::views::Manifest,
    callgraph_blob_database: &Path,
) -> Result<MaterializeStats> {
    apply_manifest_diff_profiled(
        database_path,
        base_manifest,
        new_manifest,
        callgraph_blob_database,
    )
    .map(|(stats, _)| stats)
}

pub(crate) fn apply_manifest_diff_profiled(
    database_path: &Path,
    base_manifest: &crate::views::Manifest,
    new_manifest: &crate::views::Manifest,
    callgraph_blob_database: &Path,
) -> Result<(MaterializeStats, profile::PhaseTimings)> {
    materialize(
        database_path,
        callgraph_blob_database,
        new_manifest,
        Some(base_manifest),
    )
}

pub(crate) mod profile;

const MATERIALIZATION_VERSION: &str = "4";

fn fingerprint(manifest: &crate::views::Manifest) -> Result<String> {
    let bytes = manifest
        .to_json_bytes()
        .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn materialize(
    database_path: &Path,
    callgraph_blob_database: &Path,
    manifest: &crate::views::Manifest,
    mut base: Option<&crate::views::Manifest>,
) -> Result<(MaterializeStats, profile::PhaseTimings)> {
    let mut profile = profile::PhaseTimer::new(if base.is_some() {
        "incremental"
    } else {
        "cold"
    });
    let mut connection = if base.is_some() {
        Connection::open_with_flags(database_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?
    } else {
        Connection::open(database_path)?
    };
    if base.is_none() {
        initialize_schema(&connection)?;
    }
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(base_manifest) = base {
        let recorded: Option<String> = transaction
            .query_row(
                "SELECT v FROM meta WHERE k = 'view_manifest_fingerprint'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let version: Option<String> = transaction
            .query_row(
                "SELECT v FROM meta WHERE k = 'view_materialization_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if recorded.is_none() && version.is_none() {
            // Cloned generations written before diff metadata existed must be
            // rebuilt; their graph cannot safely seed dependency invalidation.
            base = None;
        } else if recorded.as_deref() != Some(fingerprint(base_manifest)?.as_str()) {
            return Err(CallGraphStoreError::Unavailable(
                "derived manifest fingerprint mismatch; cold materialization required".into(),
            ));
        } else if version.as_deref() != Some(MATERIALIZATION_VERSION) {
            base = None;
        } else if base_manifest == manifest {
            profile.finish("load_bindings_select");
            return Ok((MaterializeStats::default(), profile.into_timings()));
        }
    }
    transaction.execute_batch("CREATE TABLE IF NOT EXISTS view_bindings (file_path TEXT PRIMARY KEY, payload TEXT NOT NULL)")?;
    let changed = base.map(|base| {
        base.entries()
            .chain(manifest.entries())
            .filter(|(path, _)| base.get(path) != manifest.get(path))
            .map(|(path, _)| path.as_bytes().to_vec())
            .collect::<BTreeSet<_>>()
    });
    let cached = if base.is_some() {
        load_bindings(&transaction)?
    } else {
        BTreeMap::new()
    };
    let selected = match (&changed, base) {
        (Some(changed), Some(base)) if !requires_full_resolution(base, manifest, changed) => {
            Some(dependent_closure(&transaction, changed)?)
        }
        _ => None,
    };
    let mut stats = MaterializeStats {
        full_resolution: selected.is_none(),
        ..MaterializeStats::default()
    };
    profile.finish("load_bindings_select");
    if let Some(changed) = &changed {
        for path in changed {
            let Ok(path) = std::str::from_utf8(path) else {
                // Non-UTF-8 entries cannot have rows in the cold materialization.
                continue;
            };
            // Edges are owned through their ref_id, not their target. Delete them
            // before the refs so that cross-file incoming edges remain available.
            stats.deleted += transaction.execute("DELETE FROM edges WHERE ref_id IN (SELECT ref_id FROM refs WHERE caller_file = ?1)", [path])?;
            stats.deleted +=
                transaction.execute("DELETE FROM refs WHERE caller_file = ?1", [path])?;
            stats.deleted +=
                transaction.execute("DELETE FROM nodes WHERE file_path = ?1", [path])?;
            stats.deleted += transaction.execute("DELETE FROM files WHERE path = ?1", [path])?;
            stats.dependency_deleted += transaction
                .execute("DELETE FROM file_dependencies WHERE file_path = ?1", [path])?;
            stats.dependency_deleted +=
                transaction.execute("DELETE FROM view_bindings WHERE file_path = ?1", [path])?;
        }
    } else {
        for table in ["edges", "refs", "nodes", "files"] {
            stats.deleted += transaction.execute(&format!("DELETE FROM {table}"), [])?;
        }
    }
    if changed.is_none() {
        for table in ["file_dependencies", "view_bindings"] {
            stats.dependency_deleted += transaction.execute(&format!("DELETE FROM {table}"), [])?;
        }
    }
    profile.finish("delete_rows");
    let blob_connection = Connection::open(callgraph_blob_database)?;
    let mut parsed = BTreeMap::new();
    let mut nodes = HashMap::new();
    let mut loaded_paths = BTreeSet::new();
    for (path, entry) in manifest.entries() {
        if changed
            .as_ref()
            .is_some_and(|paths| !paths.contains(path.as_bytes()))
        {
            continue;
        }
        let crate::views::ManifestEntry::Regular {
            planes,
            resolution_input,
            ..
        } = entry
        else {
            continue;
        };
        let Some(key) = planes.callgraph.as_deref() else {
            continue;
        };
        let key_bytes = decode_manifest_full_key(key).ok_or_else(|| {
            CallGraphStoreError::Unavailable(format!("invalid manifest callgraph key {key}"))
        })?;
        let payload = blob_connection
            .query_row(
                "SELECT payload FROM blob_payloads WHERE full_key = ?1",
                [key_bytes],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .ok_or_else(|| {
                CallGraphStoreError::Unavailable(format!("missing manifest callgraph blob {key}"))
            })?;
        let blob = join::CallgraphBlob::from_bytes(&payload)
            .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
        let Some(parse) = blob.parse() else {
            continue;
        };
        let path = String::from_utf8(path.as_bytes().to_vec()).map_err(|_| {
            CallGraphStoreError::Unavailable("non-UTF-8 manifest callgraph path".to_string())
        })?;
        loaded_paths.insert(path.clone());
        let write_owned = changed
            .as_ref()
            .is_none_or(|paths| paths.contains(path.as_bytes()));
        if write_owned {
            stats.inserted += transaction.execute(
                "INSERT OR REPLACE INTO files
             (path, content_hash, mtime_ns, size, lang, is_dead_code_root, is_public_api,
              surface_fingerprint, indexed_at)
             VALUES (?1, ?2, 0, 0, ?3, 0, 0, '', 0)",
                params![path, key, parse.language],
            )?;
        }
        for symbol in &parse.symbols {
            let id = format!("view:{path}:{}:{}", symbol.scoped_name, symbol.ordinal);
            if write_owned {
                stats.inserted += transaction.execute(
                    "INSERT OR REPLACE INTO nodes
                 (id, file_path, name, scoped_name, kind, start_line, start_col, end_line,
                  end_col, range_ordinal, signature, exported, is_default_export,
                  is_type_like, is_callgraph_entry_point, provenance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 0, ?12, ?14)",
                    params![
                        id,
                        path,
                        symbol.name,
                        symbol.scoped_name,
                        symbol.kind,
                        i64::from(symbol.start_line),
                        i64::from(symbol.start_col),
                        i64::from(symbol.end_line),
                        i64::from(symbol.end_col),
                        i64::from(symbol.ordinal),
                        symbol.signature,
                        i64::from(symbol.exported),
                        i64::from(symbol.is_default_export),
                        PROVENANCE_TREESITTER,
                    ],
                )?;
            }
            nodes.insert((path.clone(), symbol.scoped_name.clone()), id.clone());
            nodes
                .entry((path.clone(), symbol.name.clone()))
                .or_insert(id);
        }
        if !resolution_input {
            parsed.insert(
                path,
                parse
                    .refs
                    .iter()
                    .fold(BTreeMap::new(), |mut by_ordinal, reference| {
                        // Match the cold writer's original first-reference lookup
                        // when structural references share an AST ordinal.
                        by_ordinal
                            .entry(reference.ordinal)
                            .or_insert_with(|| reference.clone());
                        by_ordinal
                    }),
            );
        }
    }

    profile.finish("owned_blob_decode_and_insert");
    let reader = ManifestViewBlobReader {
        connection: &blob_connection,
    };
    let changed_strings = changed.as_ref().map_or_else(BTreeSet::new, |paths| {
        paths
            .iter()
            .filter_map(|path| String::from_utf8(path.clone()).ok())
            .collect()
    });
    let membership_changed = base.map_or_else(BTreeSet::new, |base| {
        base.entries()
            .chain(manifest.entries())
            .filter(|(path, _)| {
                base.get(path).map(crate::views::ManifestEntry::kind)
                    != manifest.get(path).map(crate::views::ManifestEntry::kind)
            })
            .filter_map(|(path, _)| String::from_utf8(path.as_bytes().to_vec()).ok())
            .collect()
    });
    let joined = join::join_selected_manifest_reusing_surfaces(
        manifest,
        &reader,
        selected.as_ref(),
        &cached,
        &changed_strings,
        &membership_changed,
    )
    .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
    profile.finish("selected_join");
    stats.rebuilt_surface_entries = joined.rebuilt_surface_entries;
    stats.decoded_caller_blobs = joined.decoded_caller_blobs;
    stats.resolved_refs = joined.result.rows.len();
    stats.resolved_files = joined.resolved_callers.len();
    stats.dependent_files = joined
        .resolved_callers
        .iter()
        .filter(|path| !changed_strings.contains(*path) && changed.is_some())
        .count();
    for (path, binding) in &joined.bindings {
        let old = cached.get(path).filter(|_| {
            changed
                .as_ref()
                .is_some_and(|paths| !paths.contains(path.as_bytes()))
        });
        if old == Some(binding) {
            continue;
        }
        let empty = BTreeSet::new();
        let previous = old.map_or(&empty, |old| &old.dependencies);
        for dependency in previous.difference(&binding.dependencies) {
            stats.dependency_deleted += transaction.execute(
                "DELETE FROM file_dependencies WHERE file_path=?1 AND dep_file=?2",
                params![path, dependency],
            )?;
        }
        for dependency in binding.dependencies.difference(previous) {
            stats.dependency_inserted += transaction.execute(
                "INSERT INTO file_dependencies(file_path, dep_file) VALUES(?1, ?2)",
                params![path, dependency],
            )?;
        }
        stats.dependency_deleted +=
            transaction.execute("DELETE FROM view_bindings WHERE file_path=?1", [path])?;
        let payload = serde_json::to_string(binding)
            .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
        stats.dependency_inserted += transaction.execute(
            "INSERT INTO view_bindings(file_path, payload) VALUES(?1, ?2)",
            params![path, payload],
        )?;
    }
    profile.finish("write_bindings");
    for row in joined.result.rows {
        let caller_path = String::from_utf8(row.caller_path.clone()).map_err(|_| {
            CallGraphStoreError::Unavailable("non-UTF-8 manifest caller path".to_string())
        })?;
        ensure_manifest_path(
            &caller_path,
            manifest,
            &blob_connection,
            &mut loaded_paths,
            &mut parsed,
            &mut nodes,
        )?;
        if let Some(target) = row
            .target_path
            .as_ref()
            .and_then(|path| std::str::from_utf8(path).ok())
        {
            ensure_manifest_path(
                target,
                manifest,
                &blob_connection,
                &mut loaded_paths,
                &mut parsed,
                &mut nodes,
            )?;
        }
        let Some(parse) = parsed.get(&caller_path) else {
            continue;
        };
        let Some(reference) = parse.get(&row.ref_ordinal) else {
            continue;
        };
        let caller_node = reference
            .caller_symbol
            .as_ref()
            .and_then(|symbol| nodes.get(&(caller_path.clone(), symbol.clone())))
            .cloned();
        let target_path = row
            .target_path
            .as_ref()
            .and_then(|path| String::from_utf8(path.clone()).ok());
        let target_symbol = row.target_symbol.clone();
        let target_node = target_path
            .as_ref()
            .zip(target_symbol.as_ref())
            .and_then(|(path, symbol)| nodes.get(&(path.clone(), symbol.clone())))
            .cloned();
        let ref_id = format!("view:{caller_path}:{}", row.ref_ordinal);
        let relink = changed
            .as_ref()
            .is_some_and(|paths| !paths.contains(caller_path.as_bytes()));
        let status = if row.status == join::ResolutionStatus::Resolved {
            "resolved"
        } else {
            "unresolved"
        };
        if relink {
            // Symbol IDs encode path, scoped name and ordinal. Even an unchanged
            // caller must be re-linked when target ordinals or resolution change.
            // Resolve against the complete new manifest: additions, reexports and
            // configuration changes can affect callers with no previous target.
            let same: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM refs WHERE ref_id = ?1 AND caller_node IS ?2
                 AND status = ?3 AND target_node IS ?4 AND target_file IS ?5 AND target_symbol IS ?6)",
                params![ref_id, caller_node, status, target_node, target_path, target_symbol],
                |row| row.get(0),
            )?;
            if same {
                continue;
            }
            stats.relinked_deleted +=
                transaction.execute("DELETE FROM edges WHERE ref_id = ?1", [&ref_id])?;
            stats.relinked_deleted +=
                transaction.execute("DELETE FROM refs WHERE ref_id = ?1", [&ref_id])?;
        }
        let inserted = if relink {
            &mut stats.relinked_inserted
        } else {
            &mut stats.inserted
        };
        *inserted += transaction.execute(
            "INSERT OR REPLACE INTO refs
             (ref_id, caller_node, caller_file, kind, short_name, full_ref, module_path,
              import_kind, local_name, requested_name, namespace_alias, wildcard, line,
              byte_start, byte_end, status, target_node, target_file, target_symbol, provenance)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                ref_id,
                caller_node,
                caller_path,
                manifest_ref_kind(row.kind),
                reference.short_name,
                reference.full_ref,
                reference.module_path,
                reference.import_kind,
                reference.local_name,
                reference.requested_name,
                reference.namespace_alias,
                i64::from(reference.wildcard),
                i64::from(reference.line),
                reference.byte_start as i64,
                reference.byte_end as i64,
                if row.status == join::ResolutionStatus::Resolved {
                    "resolved"
                } else {
                    "unresolved"
                },
                target_node,
                target_path,
                target_symbol,
                PROVENANCE_TREESITTER,
            ],
        )?;
        if row.kind == join::BlobRefKind::Call {
            if let (Some(source_node), Some(target_file), Some(target_symbol)) =
                (caller_node, target_path, target_symbol)
            {
                *inserted += transaction.execute(
                    "INSERT OR REPLACE INTO edges
                     (edge_id, ref_id, source_node, target_node, target_file, target_symbol,
                      kind, line, provenance)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'call', ?7, ?8)",
                    params![
                        format!("edge:{ref_id}"),
                        ref_id,
                        source_node,
                        target_node,
                        target_file,
                        target_symbol,
                        i64::from(reference.line),
                        PROVENANCE_TREESITTER,
                    ],
                )?;
            }
        }
    }
    profile.finish("emit_refs_edges");
    set_meta_ready(&transaction, true)?;
    transaction.execute(
        "INSERT OR REPLACE INTO meta(k, v) VALUES('view_manifest_fingerprint', ?1)",
        [fingerprint(manifest)?],
    )?;
    transaction.execute(
        "INSERT OR REPLACE INTO meta(k, v) VALUES('view_materialization_version', ?1)",
        [MATERIALIZATION_VERSION],
    )?;
    transaction.commit()?;
    profile.finish("commit");
    Ok((stats, profile.into_timings()))
}

struct ManifestViewBlobReader<'a> {
    connection: &'a Connection,
}

impl join::ManifestBlobReader for ManifestViewBlobReader<'_> {
    fn read_callgraph_blob(
        &self,
        full_key: &str,
    ) -> std::result::Result<Option<Vec<u8>>, join::ManifestJoinError> {
        let Some(key) = decode_manifest_full_key(full_key) else {
            return Ok(None);
        };
        self.connection
            .query_row(
                "SELECT payload FROM blob_payloads WHERE full_key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| join::ManifestJoinError::InvalidBlob(error.to_string()))
    }
}

fn decode_manifest_full_key(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

const fn manifest_ref_kind(kind: join::BlobRefKind) -> &'static str {
    match kind {
        join::BlobRefKind::Call => "call",
        join::BlobRefKind::ValueRef => "value_ref",
        join::BlobRefKind::Import => "import",
        join::BlobRefKind::Module => "module",
        join::BlobRefKind::Reexport => "reexport",
        join::BlobRefKind::ExportAlias => "export_alias",
    }
}

#[cfg(test)]
mod tests;

fn load_bindings(
    connection: &Connection,
) -> Result<BTreeMap<String, join::ViewBindingDependencies>> {
    let mut statement = connection.prepare("SELECT file_path, payload FROM view_bindings")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (path, payload) = row?;
        let binding = serde_json::from_str(&payload)
            .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
        Ok((path, binding))
    })
    .collect()
}

fn dependent_closure(
    connection: &Connection,
    changed: &BTreeSet<Vec<u8>>,
) -> Result<BTreeSet<String>> {
    let mut selected = changed
        .iter()
        .filter_map(|path| String::from_utf8(path.clone()).ok())
        .collect::<BTreeSet<_>>();
    if !changed.is_empty() {
        // Rust inline-module and parent queries can inspect the crate-wide index.
        // Recheck that domain even for non-.rs paths: explicit module paths may
        // name other extensions, and membership changes can expose new modules.
        selected.insert(join::VIEW_RUST_MODULE_DOMAIN.to_string());
    }
    let mut pending = selected.iter().cloned().collect::<Vec<_>>();
    let mut dependents =
        connection.prepare("SELECT file_path FROM file_dependencies WHERE dep_file = ?1")?;
    while let Some(path) = pending.pop() {
        for caller in dependents.query_map([path], |row| row.get::<_, String>(0))? {
            let caller = caller?;
            if selected.insert(caller.clone()) {
                pending.push(caller);
            }
        }
    }
    Ok(selected)
}

fn requires_full_resolution(
    base: &crate::views::Manifest,
    next: &crate::views::Manifest,
    changed: &BTreeSet<Vec<u8>>,
) -> bool {
    changed.iter().any(|path| {
        join::view_resolution_config(path)
            || base
                .entries()
                .chain(next.entries())
                .any(|(candidate, entry)| {
                    candidate.as_bytes() == path
                        && matches!(
                            entry,
                            crate::views::ManifestEntry::Regular {
                                resolution_input: true,
                                ..
                            } | crate::views::ManifestEntry::Synthetic { .. }
                                | crate::views::ManifestEntry::Symlink { .. }
                                | crate::views::ManifestEntry::Gitlink { .. }
                        )
                })
    })
}

/// Unchanged blobs are decoded for row emission only when a selected reference
/// actually needs their caller data or target IDs. The join builds its own index;
/// decoding every blob again here would erase much of the incremental saving.
fn ensure_manifest_path(
    path: &str,
    manifest: &crate::views::Manifest,
    blobs: &Connection,
    loaded: &mut BTreeSet<String>,
    parsed: &mut BTreeMap<String, BTreeMap<u32, join::BlobRef>>,
    nodes: &mut HashMap<(String, String), String>,
) -> Result<()> {
    if !loaded.insert(path.to_string()) {
        return Ok(());
    }
    let Ok(rel) = crate::views::RelPath::new(path.as_bytes().to_vec()) else {
        return Ok(());
    };
    let Some(crate::views::ManifestEntry::Regular {
        planes,
        resolution_input,
        ..
    }) = manifest.get(&rel)
    else {
        return Ok(());
    };
    let Some(key) = &planes.callgraph else {
        return Ok(());
    };
    let key_bytes = decode_manifest_full_key(key).ok_or_else(|| {
        CallGraphStoreError::Unavailable(format!("invalid manifest callgraph key {key}"))
    })?;
    let payload = blobs
        .query_row(
            "SELECT payload FROM blob_payloads WHERE full_key=?1",
            [key_bytes],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
        .ok_or_else(|| {
            CallGraphStoreError::Unavailable(format!("missing manifest callgraph blob {key}"))
        })?;
    let blob = join::CallgraphBlob::from_bytes(&payload)
        .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
    let Some(parse) = blob.parse() else {
        return Ok(());
    };
    for symbol in &parse.symbols {
        let id = format!("view:{path}:{}:{}", symbol.scoped_name, symbol.ordinal);
        nodes.insert((path.to_string(), symbol.scoped_name.clone()), id.clone());
        nodes
            .entry((path.to_string(), symbol.name.clone()))
            .or_insert(id);
    }
    if !resolution_input {
        parsed.insert(
            path.to_string(),
            parse
                .refs
                .iter()
                .fold(BTreeMap::new(), |mut by_ordinal, reference| {
                    by_ordinal
                        .entry(reference.ordinal)
                        .or_insert_with(|| reference.clone());
                    by_ordinal
                }),
        );
    }
    Ok(())
}
