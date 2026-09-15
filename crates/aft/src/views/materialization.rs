use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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
    pub delete_paths_touched: usize,
    pub deleted: usize,
    pub inserted: usize,
    pub relinked_deleted: usize,
    pub relinked_inserted: usize,
    pub dependency_deleted: usize,
    pub dependency_inserted: usize,
    pub surface_deleted: usize,
    pub surface_inserted: usize,
    pub dependent_files: usize,
    pub resolved_files: usize,
    pub resolved_refs: usize,
    pub resolved_bindings: usize,
    pub rebuilt_surface_entries: usize,
    pub decoded_caller_blobs: usize,
    pub full_resolution: bool,
    pub unattributed_callers: usize,
}

impl MaterializeStats {
    pub fn graph_rows_written(&self) -> usize {
        self.deleted + self.inserted + self.relinked_deleted + self.relinked_inserted
    }

    pub fn rows_written(&self) -> usize {
        self.graph_rows_written()
            + self.dependency_deleted
            + self.dependency_inserted
            + self.surface_deleted
            + self.surface_inserted
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
mod resolution_facts;

const MATERIALIZATION_VERSION: &str = "6";

fn fingerprint(manifest: &crate::views::Manifest) -> Result<String> {
    let bytes = manifest
        .to_json_bytes()
        .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn manifest_callgraph_equivalent(
    left: &crate::views::Manifest,
    right: &crate::views::Manifest,
) -> bool {
    let mut left = left.entries();
    let mut right = right.entries();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some((left_path, left_entry)), Some((right_path, right_entry)))
                if left_path == right_path
                    && match (left_entry, right_entry) {
                        (
                            crate::views::ManifestEntry::Regular {
                                mode: left_mode,
                                planes: left_planes,
                                resolution_input: left_resolution_input,
                            },
                            crate::views::ManifestEntry::Regular {
                                mode: right_mode,
                                planes: right_planes,
                                resolution_input: right_resolution_input,
                            },
                        ) => {
                            left_mode == right_mode
                                && left_resolution_input == right_resolution_input
                                && left_planes.callgraph == right_planes.callgraph
                        }
                        _ => left_entry == right_entry,
                    } => {}
            _ => return false,
        }
    }
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
    configure_materialization_connection(&connection)?;
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
        } else if manifest_callgraph_equivalent(base_manifest, manifest) {
            profile.finish("load_bindings_select");
            transaction.execute(
                "INSERT OR REPLACE INTO meta(k, v) VALUES('view_manifest_fingerprint', ?1)",
                [fingerprint(manifest)?],
            )?;
            transaction.commit()?;
            profile.finish("commit");
            return Ok((MaterializeStats::default(), profile.into_timings()));
        }
    }
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS view_bindings (
             file_path TEXT PRIMARY KEY,
             payload TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS view_file_surfaces (
             file_path TEXT PRIMARY KEY,
             payload TEXT NOT NULL
         )",
    )?;
    let changed = base.map(|base| {
        base.entries()
            .chain(manifest.entries())
            .filter(|(path, _)| base.get(path) != manifest.get(path))
            .map(|(path, _)| path.as_bytes().to_vec())
            .collect::<BTreeSet<_>>()
    });
    let blob_connection = Connection::open(callgraph_blob_database)?;
    let reader = ManifestViewBlobReader::new(&blob_connection);
    let mut cached_for_invalidation = None;
    let mut fact_invalidated = BTreeSet::new();
    let mut fallback_count = 0;
    let selected = match (&changed, base) {
        (Some(changed), Some(base)) if !requires_full_resolution(base, manifest, changed) => {
            let diff = resolution_facts::diff_inputs(base, manifest, changed, &blob_connection)?;
            if diff.unknown == 0 && (!diff.changed.is_empty() || diff.inputs_changed) {
                let cached = load_bindings(&transaction)?;
                for (caller, binding) in &cached {
                    if !binding.consulted_facts.is_disjoint(&diff.changed) {
                        fact_invalidated.insert(caller.clone());
                    }
                    if diff.inputs_changed && binding.unattributed {
                        fallback_count += 1;
                    }
                }
                cached_for_invalidation = Some(cached);
            }
            if diff.unknown != 0 || fallback_count != 0 {
                fallback_count += diff.unknown;
                log::warn!("views materialization: full resolution (reason=unattributed_reads count={fallback_count})");
                None
            } else {
                // Configuration bytes are invalidated by their consulted fields.
                // Presence changes still seed ordinary missing-path dependencies.
                let mut seeds = changed
                    .iter()
                    .filter(|path| {
                        !join::view_resolution_config(path) || {
                            let rel =
                                crate::views::RelPath::new((*path).clone()).expect("manifest path");
                            base.get(&rel).is_some() != manifest.get(&rel).is_some()
                        }
                    })
                    .cloned()
                    .collect::<BTreeSet<_>>();
                seeds.extend(fact_invalidated.iter().map(|path| path.as_bytes().to_vec()));
                if changed.iter().any(|path| {
                    join::view_resolution_config(path) && {
                        let rel = crate::views::RelPath::new(path.clone()).expect("manifest path");
                        base.get(&rel).is_some() != manifest.get(&rel).is_some()
                    }
                }) {
                    seeds.insert(join::VIEW_CONFIG_MEMBERSHIP_DOMAIN.as_bytes().to_vec());
                }
                Some(dependent_closure(&transaction, &seeds)?)
            }
        }
        _ => None,
    };
    let cached = if base.is_none() {
        BTreeMap::new()
    } else if let Some(cached) = cached_for_invalidation {
        cached
    } else if let Some(selected) = &selected {
        load_bindings_for_selection(
            &transaction,
            selected,
            changed.as_ref().expect("incremental selection has a diff"),
        )?
    } else {
        load_bindings(&transaction)?
    };
    let mut stats = MaterializeStats {
        full_resolution: selected.is_none(),
        unattributed_callers: fallback_count,
        ..MaterializeStats::default()
    };
    profile.finish("load_bindings_select");
    if let Some(changed) = &changed {
        transaction.execute_batch(
            "CREATE TEMP TABLE changed_view_paths (
                 path TEXT PRIMARY KEY
             ) WITHOUT ROWID",
        )?;
        {
            let mut insert =
                transaction.prepare("INSERT INTO changed_view_paths(path) VALUES(?1)")?;
            for path in changed {
                let Ok(path) = std::str::from_utf8(path) else {
                    // Non-UTF-8 entries cannot have rows in the cold materialization.
                    continue;
                };
                stats.delete_paths_touched += insert.execute([path])?;
            }
        }
        // Edges are owned through their ref_id, not their target. Delete them
        // before refs so cross-file incoming edges remain available. Starting
        // every lookup from the bounded path table keeps deletion on the diff.
        stats.deleted += transaction.execute(
            "DELETE FROM edges WHERE ref_id IN (
                 SELECT refs.ref_id FROM changed_view_paths
                 CROSS JOIN refs INDEXED BY idx_refs_caller_file
                     ON refs.caller_file = changed_view_paths.path
             )",
            [],
        )?;
        stats.deleted += transaction.execute(
            "DELETE FROM refs WHERE caller_file IN (SELECT path FROM changed_view_paths)",
            [],
        )?;
        stats.deleted += transaction.execute(
            "DELETE FROM nodes WHERE file_path IN (SELECT path FROM changed_view_paths)",
            [],
        )?;
        stats.deleted += transaction.execute(
            "DELETE FROM files WHERE path IN (SELECT path FROM changed_view_paths)",
            [],
        )?;
        stats.dependency_deleted += transaction.execute(
            "DELETE FROM file_dependencies WHERE file_path IN (SELECT path FROM changed_view_paths)",
            [],
        )?;
        stats.dependency_deleted += transaction.execute(
            "DELETE FROM view_bindings WHERE file_path IN (SELECT path FROM changed_view_paths)",
            [],
        )?;
        stats.surface_deleted += transaction.execute(
            "DELETE FROM view_file_surfaces WHERE file_path IN (SELECT path FROM changed_view_paths)",
            [],
        )?;
    } else {
        for table in ["edges", "refs", "nodes", "files"] {
            stats.deleted += transaction.execute(&format!("DELETE FROM {table}"), [])?;
        }
    }
    if changed.is_none() {
        for table in ["file_dependencies", "view_bindings"] {
            stats.dependency_deleted += transaction.execute(&format!("DELETE FROM {table}"), [])?;
        }
        stats.surface_deleted += transaction.execute("DELETE FROM view_file_surfaces", [])?;
    }
    profile.finish("delete_rows");
    let mut parsed = BTreeMap::new();
    let mut nodes = HashMap::<String, HashMap<String, String>>::new();
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
        let blob = reader
            .read_decoded(key)
            .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?
            .ok_or_else(|| {
                CallGraphStoreError::Unavailable(format!("missing manifest callgraph blob {key}"))
            })?;
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
        let file_nodes = nodes.entry(path.clone()).or_default();
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
            file_nodes.insert(symbol.scoped_name.clone(), id.clone());
            file_nodes.entry(symbol.name.clone()).or_insert(id);
        }
        if !resolution_input {
            parsed.insert(path, EmissionParse::new(blob));
        }
    }

    profile.finish("owned_blob_decode_and_insert");
    let changed_strings = changed.as_ref().map_or_else(BTreeSet::new, |paths| {
        paths
            .iter()
            .filter_map(|path| String::from_utf8(path.clone()).ok())
            .collect()
    });
    let mut membership_changed = base.map_or_else(BTreeSet::new, |base| {
        base.entries()
            .chain(manifest.entries())
            .filter(|(path, _)| {
                base.get(path).map(crate::views::ManifestEntry::kind)
                    != manifest.get(path).map(crate::views::ManifestEntry::kind)
            })
            .filter_map(|(path, _)| String::from_utf8(path.as_bytes().to_vec()).ok())
            .collect()
    });
    if membership_changed
        .iter()
        .any(|path| join::view_resolution_config(path.as_bytes()))
    {
        membership_changed.insert(join::VIEW_CONFIG_MEMBERSHIP_DOMAIN.into());
    }
    let joined = join::join_selected_manifest_reusing_surfaces(
        manifest,
        &reader,
        selected.as_ref(),
        &cached,
        &changed_strings,
        &membership_changed,
        &fact_invalidated,
    )
    .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
    profile.finish("selected_join");
    stats.unattributed_callers = joined
        .bindings
        .values()
        .filter(|binding| binding.unattributed)
        .count()
        .max(stats.unattributed_callers);
    stats.rebuilt_surface_entries = joined.rebuilt_surface_entries;
    stats.decoded_caller_blobs = joined.decoded_caller_blobs;
    stats.resolved_refs = joined.result.rows.len();
    stats.resolved_bindings = joined.resolved_bindings;
    stats.resolved_files = joined.resolved_callers.len();
    stats.dependent_files = joined
        .resolved_callers
        .iter()
        .filter(|path| !changed_strings.contains(*path) && changed.is_some())
        .count();
    for (path, binding) in &joined.bindings {
        if joined.rebuilt_surface_paths.contains(path) {
            if let Some(payload) = binding
                .surface_json()
                .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?
            {
                stats.surface_inserted += transaction.execute(
                    "INSERT OR REPLACE INTO view_file_surfaces(file_path, payload) VALUES(?1, ?2)",
                    params![path, payload],
                )?;
            }
        }
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
    // Retain prepared statements across the fan-out. Preparing each statement
    // again costs more than binding many of these small reference rows.
    {
        transaction.execute_batch(
            "CREATE TEMP TABLE pending_view_refs AS SELECT * FROM refs WHERE 0;
             CREATE TEMP TABLE pending_view_edges AS SELECT * FROM edges WHERE 0",
        )?;
        let mut same_ref = transaction.prepare("SELECT EXISTS(SELECT 1 FROM refs WHERE ref_id = ?1 AND caller_node IS ?2
                 AND status = ?3 AND target_node IS ?4 AND target_file IS ?5 AND target_symbol IS ?6)")?;
        let mut delete_edge = transaction.prepare("DELETE FROM edges WHERE ref_id = ?1")?;
        let mut delete_ref = transaction.prepare("DELETE FROM refs WHERE ref_id = ?1")?;
        let mut insert_ref = transaction.prepare(
            "INSERT INTO pending_view_refs
             (ref_id, caller_node, caller_file, kind, short_name, full_ref, module_path,
              import_kind, local_name, requested_name, namespace_alias, wildcard, line,
              byte_start, byte_end, status, target_node, target_file, target_symbol, provenance)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     ?15, ?16, ?17, ?18, ?19, ?20)",
        )?;
        let mut insert_edge = transaction.prepare(
            "INSERT INTO pending_view_edges
                     (edge_id, ref_id, source_node, target_node, target_file, target_symbol,
                      kind, line, provenance)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'call', ?7, ?8)",
        )?;
        for row in joined.result.rows {
            let caller_path = String::from_utf8(row.caller_path).map_err(|_| {
                CallGraphStoreError::Unavailable("non-UTF-8 manifest caller path".to_string())
            })?;
            ensure_manifest_path(
                &caller_path,
                manifest,
                &reader,
                &mut loaded_paths,
                &mut parsed,
                &mut nodes,
            )?;
            let target_path = row
                .target_path
                .and_then(|path| String::from_utf8(path).ok());
            if let Some(target) = target_path.as_deref() {
                ensure_manifest_path(
                    target,
                    manifest,
                    &reader,
                    &mut loaded_paths,
                    &mut parsed,
                    &mut nodes,
                )?;
            }
            let Some(parse) = parsed.get(&caller_path) else {
                continue;
            };
            let Some(reference) = parse.reference(row.ref_ordinal) else {
                continue;
            };
            let caller_node = reference
                .caller_symbol
                .as_ref()
                .and_then(|symbol| nodes.get(&caller_path)?.get(symbol))
                .cloned();
            let target_symbol = row.target_symbol;
            let target_node = target_path
                .as_ref()
                .zip(target_symbol.as_ref())
                .and_then(|(path, symbol)| nodes.get(path)?.get(symbol))
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
                let same: bool = same_ref.query_row(
                    params![
                        ref_id,
                        caller_node,
                        status,
                        target_node,
                        target_path,
                        target_symbol
                    ],
                    |row| row.get(0),
                )?;
                if same {
                    continue;
                }
                stats.relinked_deleted += delete_edge.execute([&ref_id])?;
                stats.relinked_deleted += delete_ref.execute([&ref_id])?;
            }
            let inserted = if relink {
                &mut stats.relinked_inserted
            } else {
                &mut stats.inserted
            };
            *inserted += insert_ref.execute(params![
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
            ])?;
            if row.kind == join::BlobRefKind::Call {
                if let (Some(source_node), Some(target_file), Some(target_symbol)) =
                    (caller_node, target_path, target_symbol)
                {
                    *inserted += insert_edge.execute(params![
                        format!("edge:{ref_id}"),
                        ref_id,
                        source_node,
                        target_node,
                        target_file,
                        target_symbol,
                        i64::from(reference.line),
                        PROVENANCE_TREESITTER,
                    ])?;
                }
            }
        }
        transaction.execute(
            "INSERT OR REPLACE INTO refs SELECT * FROM pending_view_refs ORDER BY rowid",
            [],
        )?;
        transaction.execute(
            "INSERT OR REPLACE INTO edges SELECT * FROM pending_view_edges ORDER BY rowid",
            [],
        )?;
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
    decoded: RefCell<HashMap<String, Arc<join::CallgraphBlob>>>,
}

impl<'a> ManifestViewBlobReader<'a> {
    fn new(connection: &'a Connection) -> Self {
        Self {
            connection,
            decoded: RefCell::new(HashMap::new()),
        }
    }

    fn read_payload(
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

    fn read_decoded(
        &self,
        full_key: &str,
    ) -> std::result::Result<Option<Arc<join::CallgraphBlob>>, join::ManifestJoinError> {
        if let Some(blob) = self.decoded.borrow().get(full_key) {
            return Ok(Some(blob.clone()));
        }
        let Some(payload) = self.read_payload(full_key)? else {
            return Ok(None);
        };
        let blob = Arc::new(join::CallgraphBlob::from_bytes(&payload)?);
        self.decoded
            .borrow_mut()
            .insert(full_key.to_string(), blob.clone());
        Ok(Some(blob))
    }
}

impl join::ManifestBlobReader for ManifestViewBlobReader<'_> {
    fn read_callgraph_blob(
        &self,
        full_key: &str,
    ) -> std::result::Result<Option<Vec<u8>>, join::ManifestJoinError> {
        self.read_payload(full_key)
    }

    fn read_callgraph_blob_decoded(
        &self,
        full_key: &str,
    ) -> std::result::Result<Option<Arc<join::CallgraphBlob>>, join::ManifestJoinError> {
        self.read_decoded(full_key)
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

fn configure_materialization_connection(connection: &Connection) -> Result<()> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    // Publication can expose the generation while its pages remain in the WAL,
    // so the transaction commit itself must survive power loss.
    connection.pragma_update(None, "synchronous", "FULL")?;
    // A detached checkpoint moves these pages into the main file. Keeping the
    // automatic threshold disabled makes that work observable and off-path.
    connection.pragma_update(None, "wal_autocheckpoint", 0)?;
    // The derived database has several secondary indexes. A publication-sized
    // page cache avoids rereading their upper levels while deleting and emitting
    // a bounded diff; temporary path/row sets never need durable spill files.
    connection.pragma_update(None, "cache_size", -65_536)?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
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
    let mut bindings = rows
        .map(|row| {
            let (path, payload) = row?;
            let binding: join::ViewBindingDependencies = serde_json::from_str(&payload)
                .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
            Ok((path, binding))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    for (path, surface) in load_surfaces(connection)? {
        if let Some(binding) = bindings.get_mut(&path) {
            binding.copy_surface_from(&surface);
        }
    }
    Ok(bindings)
}

fn load_surfaces(
    connection: &Connection,
) -> Result<BTreeMap<String, join::ViewBindingDependencies>> {
    let mut statement = connection.prepare("SELECT file_path, payload FROM view_file_surfaces")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (path, surface) = row?;
        let binding = join::ViewBindingDependencies::from_surface_json(&surface)
            .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
        Ok((path, binding))
    })
    .collect()
}

fn load_bindings_for_selection(
    connection: &Connection,
    selected: &BTreeSet<String>,
    changed: &BTreeSet<Vec<u8>>,
) -> Result<BTreeMap<String, join::ViewBindingDependencies>> {
    connection.execute_batch(
        "CREATE TEMP TABLE selected_view_paths (
             path TEXT PRIMARY KEY
         ) WITHOUT ROWID",
    )?;
    {
        let mut insert = connection.prepare("INSERT INTO selected_view_paths(path) VALUES(?1)")?;
        for path in selected {
            if !changed.contains(path.as_bytes()) {
                insert.execute([path])?;
            }
        }
    }

    let mut bindings = load_surfaces(connection)?;
    for path in changed {
        if let Ok(path) = std::str::from_utf8(path) {
            bindings.remove(path);
        }
    }
    {
        let mut statement = connection.prepare(
            "SELECT bindings.file_path, bindings.payload
             FROM selected_view_paths
             CROSS JOIN view_bindings AS bindings INDEXED BY sqlite_autoindex_view_bindings_1
                 ON bindings.file_path = selected_view_paths.path",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (path, payload) = row?;
            let mut binding: join::ViewBindingDependencies = serde_json::from_str(&payload)
                .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
            if let Some(surface) = bindings.get(&path) {
                binding.copy_surface_from(surface);
            }
            bindings.insert(path, binding);
        }
    }
    Ok(bindings)
}

fn dependent_closure(
    connection: &Connection,
    changed: &BTreeSet<Vec<u8>>,
) -> Result<BTreeSet<String>> {
    connection.execute_batch(
        "CREATE TEMP TABLE dependency_seed_paths (
             path TEXT PRIMARY KEY
         ) WITHOUT ROWID",
    )?;
    {
        let mut insert =
            connection.prepare("INSERT OR IGNORE INTO dependency_seed_paths(path) VALUES(?1)")?;
        for path in changed {
            if let Ok(path) = std::str::from_utf8(path) {
                insert.execute([path])?;
            }
        }
        if !changed.is_empty() {
            // Rust inline-module and parent queries can inspect the crate-wide index.
            // Recheck that domain even for non-.rs paths: explicit module paths may
            // name other extensions, and membership changes can expose new modules.
            insert.execute([join::VIEW_RUST_MODULE_DOMAIN])?;
        }
    }
    let mut statement = connection.prepare(
        "WITH RECURSIVE selected(path) AS (
             SELECT path FROM dependency_seed_paths
             UNION
             SELECT dependencies.file_path
             FROM selected
             JOIN file_dependencies AS dependencies INDEXED BY idx_file_dependencies_dep_file
                 ON dependencies.dep_file = selected.path
         )
         SELECT path FROM selected",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<BTreeSet<_>, _>>()
        .map_err(Into::into)
}

fn requires_full_resolution(
    base: &crate::views::Manifest,
    next: &crate::views::Manifest,
    changed: &BTreeSet<Vec<u8>>,
) -> bool {
    changed.iter().any(|path| {
        base.entries()
            .chain(next.entries())
            .any(|(candidate, entry)| {
                candidate.as_bytes() == path
                    && matches!(
                        entry,
                        crate::views::ManifestEntry::Synthetic { .. }
                            | crate::views::ManifestEntry::Symlink { .. }
                            | crate::views::ManifestEntry::Gitlink { .. }
                    )
            })
    })
}

struct EmissionParse {
    blob: Arc<join::CallgraphBlob>,
    refs_by_ordinal: HashMap<u32, usize>,
}

impl EmissionParse {
    fn new(blob: Arc<join::CallgraphBlob>) -> Self {
        let refs_by_ordinal = blob
            .parse()
            .into_iter()
            .flat_map(|parse| parse.refs.iter().enumerate())
            .fold(HashMap::new(), |mut by_ordinal, (position, reference)| {
                // Match the cold writer's original first-reference lookup when
                // structural references share an AST ordinal.
                by_ordinal.entry(reference.ordinal).or_insert(position);
                by_ordinal
            });
        Self {
            blob,
            refs_by_ordinal,
        }
    }

    fn reference(&self, ordinal: u32) -> Option<&join::BlobRef> {
        self.blob
            .parse()?
            .refs
            .get(*self.refs_by_ordinal.get(&ordinal)?)
    }
}

/// Unchanged blobs are decoded for row emission only when a selected reference
/// actually needs their caller data or target IDs. The join builds its own index;
/// decoding every blob again here would erase much of the incremental saving.
fn ensure_manifest_path(
    path: &str,
    manifest: &crate::views::Manifest,
    blobs: &ManifestViewBlobReader<'_>,
    loaded: &mut BTreeSet<String>,
    parsed: &mut BTreeMap<String, EmissionParse>,
    nodes: &mut HashMap<String, HashMap<String, String>>,
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
    let blob = blobs
        .read_decoded(key)
        .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?
        .ok_or_else(|| {
            CallGraphStoreError::Unavailable(format!("missing manifest callgraph blob {key}"))
        })?;
    let Some(parse) = blob.parse() else {
        return Ok(());
    };
    let file_nodes = nodes.entry(path.to_string()).or_default();
    for symbol in &parse.symbols {
        let id = format!("view:{path}:{}:{}", symbol.scoped_name, symbol.ordinal);
        file_nodes.insert(symbol.scoped_name.clone(), id.clone());
        file_nodes.entry(symbol.name.clone()).or_insert(id);
    }
    if !resolution_input {
        parsed.insert(path.to_string(), EmissionParse::new(blob));
    }
    Ok(())
}
