use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::callgraph_store::{
    initialize_schema, join, set_meta_ready, CallGraphStoreError, Result, PROVENANCE_TREESITTER,
};

pub(crate) fn materialize_manifest_view_database(
    database_path: &Path,
    callgraph_blob_database: &Path,
    manifest: &crate::views::Manifest,
) -> Result<()> {
    let mut connection = Connection::open(database_path)?;
    initialize_schema(&connection)?;
    let transaction = connection.transaction()?;
    for table in ["edges", "refs", "nodes", "files"] {
        transaction.execute(&format!("DELETE FROM {table}"), [])?;
    }
    let blob_connection = Connection::open(callgraph_blob_database)?;
    let mut parsed = BTreeMap::new();
    let mut nodes = HashMap::new();
    for (path, entry) in manifest.entries() {
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
        transaction.execute(
            "INSERT OR REPLACE INTO files
             (path, content_hash, mtime_ns, size, lang, is_dead_code_root, is_public_api,
              surface_fingerprint, indexed_at)
             VALUES (?1, ?2, 0, 0, ?3, 0, 0, '', 0)",
            params![path, key, parse.language],
        )?;
        for symbol in &parse.symbols {
            let id = format!("view:{path}:{}:{}", symbol.scoped_name, symbol.ordinal);
            transaction.execute(
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
            nodes.insert((path.clone(), symbol.scoped_name.clone()), id.clone());
            nodes
                .entry((path.clone(), symbol.name.clone()))
                .or_insert(id);
        }
        if !resolution_input {
            parsed.insert(path, parse.clone());
        }
    }

    let reader = ManifestViewBlobReader {
        connection: &blob_connection,
    };
    let joined = join::JoinResult::from_manifest(manifest, &reader)
        .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
    for row in joined.rows {
        let caller_path = String::from_utf8(row.caller_path.clone()).map_err(|_| {
            CallGraphStoreError::Unavailable("non-UTF-8 manifest caller path".to_string())
        })?;
        let Some(parse) = parsed.get(&caller_path) else {
            continue;
        };
        let Some(reference) = parse
            .refs
            .iter()
            .find(|reference| reference.ordinal == row.ref_ordinal)
        else {
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
        transaction.execute(
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
                transaction.execute(
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
    set_meta_ready(&transaction, true)?;
    transaction.commit()?;
    Ok(())
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

