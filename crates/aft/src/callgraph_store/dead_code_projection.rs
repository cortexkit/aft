use rusqlite::{params, Connection, OpenFlags};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::inspect::job::{CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot};
use crate::inspect::scanners::DEFAULT_EXPORT_MARKER_KIND;
use crate::symbols::SymbolKind;

use super::{
    database_ready, lang_from_label, CallGraphStoreError, Result, BACKEND_TREESITTER,
    PROVENANCE_NAME_MATCH, PROVENANCE_TYPE_MATCH, TOP_LEVEL_SYMBOL,
};

pub fn project_dead_code_snapshot(db_path: &Path) -> Result<CallgraphSnapshot> {
    if !db_path.is_file() {
        return Err(CallGraphStoreError::Unavailable(format!(
            "database does not exist: {}",
            db_path.display()
        )));
    }

    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_millis(5_000))?;
    if !database_ready(&conn).unwrap_or(false) {
        return Err(CallGraphStoreError::Unavailable(
            "database is missing, stale, or mid-build".to_string(),
        ));
    }

    let project_root = project_root_from_backend_state(&conn)?;
    if stale_backend_file_count(&conn, &project_root)? > 0 {
        return Err(CallGraphStoreError::Unavailable(
            "callgraph has stale files pending refresh".to_string(),
        ));
    }
    let mut paths = SnapshotPathResolver::new(&project_root);
    let files = project_files_from_store(&conn, &mut paths)?;
    let exported_symbols = exported_symbols_from_store(&conn, &mut paths)?;
    let outbound_calls = outbound_calls_from_store(&conn, &mut paths)?;
    let entry_points = entry_points_for_files(&project_root, &files);
    let entry_point_symbols = entry_point_symbols_from_store(&conn, &mut paths)?;

    Ok(CallgraphSnapshot {
        generated_at: Some(SystemTime::now()),
        files,
        exported_symbols,
        outbound_calls,
        entry_points,
        entry_point_symbols,
    })
}

fn project_root_from_backend_state(conn: &Connection) -> Result<PathBuf> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT workspace_root
         FROM backend_file_state
         WHERE backend = ?1
         ORDER BY workspace_root",
    )?;
    let roots = statement
        .query_map([BACKEND_TREESITTER], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    match roots.as_slice() {
        [root] => Ok(PathBuf::from(root)),
        [] => Err(CallGraphStoreError::Unavailable(
            "database has no workspace root rows".to_string(),
        )),
        _ => Err(CallGraphStoreError::Unavailable(format!(
            "database has multiple workspace roots: {}",
            roots.join(", ")
        ))),
    }
}

fn stale_backend_file_count(conn: &Connection, project_root: &Path) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM backend_file_state
         WHERE backend = ?1 AND workspace_root = ?2 AND status = 'stale'",
        params![BACKEND_TREESITTER, project_root.display().to_string()],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn project_files_from_store(
    conn: &Connection,
    paths: &mut SnapshotPathResolver<'_>,
) -> Result<Vec<PathBuf>> {
    let mut statement = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let files = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|path| path.map(|path| paths.resolve(&path)))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(files)
}

fn exported_symbols_from_store(
    conn: &Connection,
    paths: &mut SnapshotPathResolver<'_>,
) -> Result<Vec<CallgraphExport>> {
    let mut statement = conn.prepare(
        "SELECT file_path, name, kind, start_line, exported, is_default_export
         FROM nodes
         WHERE exported != 0 OR is_default_export != 0
         ORDER BY file_path, start_line, name, kind, id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(ExportRow {
            file_path: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            line: (row.get::<_, i64>(3)?.max(0) as u32).saturating_add(1),
            exported: row.get::<_, i64>(4)? != 0,
            is_default_export: row.get::<_, i64>(5)? != 0,
        })
    })?;

    let mut exports = Vec::new();
    for row in rows {
        let row = row?;
        let file = paths.resolve(&row.file_path);
        if row.exported {
            exports.push(CallgraphExport {
                file: file.clone(),
                symbol: row.name.clone(),
                kind: row.kind,
                line: row.line,
            });
        }
        if row.is_default_export {
            exports.push(CallgraphExport {
                file,
                symbol: row.name,
                kind: DEFAULT_EXPORT_MARKER_KIND.to_string(),
                line: row.line,
            });
        }
    }
    Ok(exports)
}

fn entry_point_symbols_from_store(
    conn: &Connection,
    paths: &mut SnapshotPathResolver<'_>,
) -> Result<BTreeMap<PathBuf, BTreeSet<String>>> {
    let mut statement = conn.prepare(
        "SELECT n.file_path, n.name, n.scoped_name, n.kind, n.exported, f.lang
         FROM nodes n
         JOIN files f ON f.path = n.file_path
         WHERE n.is_callgraph_entry_point != 0
         ORDER BY n.file_path, n.start_line, n.name, n.kind, n.id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(EntryPointSymbolRow {
            file_path: row.get(0)?,
            name: row.get(1)?,
            scoped_name: row.get(2)?,
            kind: row.get(3)?,
            exported: row.get::<_, i64>(4)? != 0,
            lang: row.get(5)?,
        })
    })?;

    let mut by_file: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    for row in rows {
        let row = row?;
        let Some(kind) = symbol_kind_from_label(&row.kind) else {
            continue;
        };
        let Some(lang) = lang_from_label(&row.lang) else {
            continue;
        };
        if crate::callgraph::is_entry_point(&row.scoped_name, &kind, row.exported, lang) {
            continue;
        }

        let file = paths.resolve(&row.file_path);
        let roots = by_file.entry(file).or_default();
        roots.insert(row.name.clone());
        if row.scoped_name != row.name {
            roots.insert(row.scoped_name);
        }
    }
    Ok(by_file)
}

fn outbound_calls_from_store(
    conn: &Connection,
    paths: &mut SnapshotPathResolver<'_>,
) -> Result<Vec<CallgraphOutboundCall>> {
    let mut statement = conn.prepare(
        "SELECT r.caller_file,
                r.caller_node,
                n.name,
                r.short_name,
                r.full_ref,
                r.status,
                COALESCE(r.target_file, e.target_file),
                COALESCE(tn.name, r.target_symbol, e.target_symbol),
                r.line,
                COALESCE(e.provenance, r.provenance)
         FROM refs r
         LEFT JOIN nodes n ON n.id = r.caller_node
         LEFT JOIN edges e ON e.ref_id = r.ref_id AND e.kind = 'call'
         LEFT JOIN nodes tn ON tn.id = e.target_node
         WHERE r.kind = 'call'
         ORDER BY r.caller_file, n.name, r.line, r.byte_start, r.byte_end, r.ref_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(OutboundRow {
            caller_file: row.get(0)?,
            caller_node: row.get(1)?,
            caller_symbol: row.get(2)?,
            short_name: row.get(3)?,
            full_ref: row.get(4)?,
            status: row.get(5)?,
            target_file: row.get(6)?,
            target_symbol: row.get(7)?,
            line: row.get::<_, i64>(8)? as u32,
            provenance: row.get(9)?,
        })
    })?;

    let mut calls = Vec::new();
    let mut stale_caller_nodes = 0usize;
    for row in rows {
        let row = row?;
        let caller_file = paths.resolve(&row.caller_file);
        let (caller_symbol, stale_caller_node) = caller_symbol_from_row(&row);
        if stale_caller_node {
            stale_caller_nodes += 1;
        }
        let short_name = row
            .short_name
            .as_deref()
            .or(row.full_ref.as_deref())
            .unwrap_or_default();
        let mut target = if is_resolved_edge(&row.status, Some(row.provenance.as_str())) {
            match (row.target_file.as_deref(), row.target_symbol.as_deref()) {
                (Some(target_file), Some(target_symbol)) => {
                    let target_file = paths.resolve(target_file);
                    format!("{}::{target_symbol}", target_file.display())
                }
                _ => short_name.to_string(),
            }
        } else {
            short_name.to_string()
        };

        if row
            .full_ref
            .as_deref()
            .is_some_and(|full_ref| is_method_dispatch_callee(full_ref, short_name))
        {
            target.push(crate::inspect::job::DISPATCHED_CALLEE_SEPARATOR);
            target.push_str(row.full_ref.as_deref().unwrap_or_default());
        }

        calls.push(CallgraphOutboundCall {
            caller_file,
            caller_symbol,
            target,
            line: row.line,
            provenance: row.provenance,
        });
    }
    if stale_caller_nodes > 0 {
        crate::slog_info!(
            "dead_code projection: {} refs had stale caller nodes (fell back to <top-level>)",
            stale_caller_nodes
        );
    }
    Ok(calls)
}

fn entry_points_for_files(project_root: &Path, files: &[PathBuf]) -> BTreeSet<PathBuf> {
    let resolved_entry_points = crate::inspect::resolve_entry_points(project_root);
    // CI probe instrumentation (probe branch only): name every path spelling
    // feeding the membership decision so the Windows-only miss is attributable.
    eprintln!("PROBE projection root = {}", project_root.display());
    for file in files {
        eprintln!(
            "PROBE file = {} | is_entry_point = {}",
            file.display(),
            resolved_entry_points.is_entry_point(file)
        );
    }
    files
        .iter()
        .filter(|file| resolved_entry_points.is_entry_point(file))
        .cloned()
        .collect()
}

fn caller_symbol_from_row(row: &OutboundRow) -> (String, bool) {
    if let Some(symbol) = &row.caller_symbol {
        return (symbol.clone(), false);
    }

    // Legacy CallGraph treats top-level calls as coming from the synthetic
    // `<top-level>` caller. A stale store row can point at a caller_node that no
    // longer exists in `nodes` (refs and nodes are refreshed by different
    // passes), so preserve the edge rather than failing the whole projection.
    (TOP_LEVEL_SYMBOL.to_string(), row.caller_node.is_some())
}

fn is_resolved_edge(status: &str, provenance: Option<&str>) -> bool {
    matches!(status, "resolved" | "resolved_local")
        || provenance.is_some_and(|provenance| {
            provenance == PROVENANCE_TYPE_MATCH
                || (provenance != PROVENANCE_NAME_MATCH
                    && (provenance.contains("treesitter") || provenance.contains("resolver")))
        })
}

fn is_method_dispatch_callee(full_callee: &str, callee_name: &str) -> bool {
    let full_callee = full_callee.trim();
    if !full_callee.contains('.') || full_callee == callee_name.trim() {
        return false;
    }

    full_callee
        .rsplit('.')
        .next()
        .map(|segment| segment.trim().trim_start_matches('?') == callee_name.trim())
        .unwrap_or(false)
}

struct SnapshotPathResolver<'a> {
    project_root: &'a Path,
    cache: HashMap<String, PathBuf>,
}

impl<'a> SnapshotPathResolver<'a> {
    fn new(project_root: &'a Path) -> Self {
        Self {
            project_root,
            cache: HashMap::new(),
        }
    }

    fn resolve(&mut self, store_path: &str) -> PathBuf {
        if let Some(path) = self.cache.get(store_path) {
            return path.clone();
        }
        let path = canonicalize_for_snapshot(&absolute_store_path(self.project_root, store_path));
        self.cache.insert(store_path.to_string(), path.clone());
        path
    }
}

fn absolute_store_path(project_root: &Path, store_path: &str) -> PathBuf {
    let path = Path::new(store_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn canonicalize_for_snapshot(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| crate::inspect::job::normalize_path(path))
}

#[derive(Debug)]
struct ExportRow {
    file_path: String,
    name: String,
    kind: String,
    line: u32,
    exported: bool,
    is_default_export: bool,
}

#[derive(Debug)]
struct EntryPointSymbolRow {
    file_path: String,
    name: String,
    scoped_name: String,
    kind: String,
    exported: bool,
    lang: String,
}

fn symbol_kind_from_label(label: &str) -> Option<SymbolKind> {
    match label {
        "function" => Some(SymbolKind::Function),
        "class" => Some(SymbolKind::Class),
        "method" => Some(SymbolKind::Method),
        "struct" => Some(SymbolKind::Struct),
        "interface" => Some(SymbolKind::Interface),
        "enum" => Some(SymbolKind::Enum),
        "type_alias" => Some(SymbolKind::TypeAlias),
        "variable" => Some(SymbolKind::Variable),
        "heading" => Some(SymbolKind::Heading),
        "file_summary" => Some(SymbolKind::FileSummary),
        _ => None,
    }
}

#[derive(Debug)]
struct OutboundRow {
    caller_file: String,
    caller_node: Option<String>,
    caller_symbol: Option<String>,
    short_name: Option<String>,
    full_ref: Option<String>,
    status: String,
    target_file: Option<String>,
    target_symbol: Option<String>,
    line: u32,
    provenance: String,
}

#[cfg(test)]
mod tests {
    use super::super::{
        CallGraphStore, PROVENANCE_NAME_MATCH, PROVENANCE_TREESITTER, PROVENANCE_TYPE_MATCH,
    };
    use super::*;
    use std::fs;

    fn assert_send<T: Send>() {}

    #[test]
    fn projection_result_is_send() {
        assert_send::<Result<CallgraphSnapshot>>();
    }

    #[test]
    fn dead_code_projection_rejects_stale_backend_rows_after_fresh_control() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join("project");
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");
        let source = src_dir.join("lib.ts");
        fs::write(
            &source,
            r#"export function used(): number {
  return 1;
}
"#,
        )
        .expect("write fixture");
        let root = fs::canonicalize(&root).expect("canonical project root");
        let source = fs::canonicalize(&source).expect("canonical source");

        let store = CallGraphStore::open(root.join(".store"), root).expect("open store");
        store
            .cold_build(std::slice::from_ref(&source))
            .expect("cold build fixture");
        let snapshot = project_dead_code_snapshot(store.sqlite_path())
            .expect("fresh backend rows should project");
        assert_eq!(snapshot.files.len(), 1);

        let marked = store
            .mark_files_stale(std::slice::from_ref(&source))
            .expect("mark source stale");
        assert_eq!(marked, vec!["src/lib.ts".to_string()]);
        let error = project_dead_code_snapshot(store.sqlite_path())
            .expect_err("stale backend rows should block projection");
        match error {
            CallGraphStoreError::Unavailable(message) => assert_eq!(
                message, "callgraph has stale files pending refresh",
                "stale projection should report a clear unavailable reason"
            ),
            other => panic!("expected Unavailable for stale backend rows, got {other:?}"),
        }
    }

    #[test]
    fn type_match_constructor_target_is_file_qualified_for_dead_code() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join("project");
        fs::create_dir_all(&root).expect("create project root");
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");
        let source = src_dir.join("lib.rs");
        let target_source = src_dir.join("other.rs");
        fs::write(
            &source,
            r#"mod other;
use other::OtherType;

fn run() {
    let _ = OtherType::new();
}
"#,
        )
        .expect("write type-match caller fixture");
        fs::write(
            &target_source,
            r#"pub struct OtherType;
impl OtherType {
    pub fn new() -> Self { Self }
}
"#,
        )
        .expect("write type-match constructor fixture");

        let store = CallGraphStore::open(root.join(".store"), root.clone()).expect("open store");
        store
            .cold_build(&[source.clone(), target_source.clone()])
            .expect("cold build type-match constructor fixture");
        let snapshot = project_dead_code_snapshot(store.sqlite_path()).expect("project snapshot");
        let expected_target = format!(
            "{}::new",
            std::fs::canonicalize(&target_source)
                .expect("canonical target source")
                .display()
        );
        let type_match_calls = snapshot
            .outbound_calls
            .iter()
            .filter(|call| call.provenance == PROVENANCE_TYPE_MATCH)
            .collect::<Vec<_>>();

        assert_eq!(
            type_match_calls.len(),
            1,
            "expected one type_match constructor call; calls: {:#?}",
            snapshot.outbound_calls
        );
        assert_eq!(type_match_calls[0].target, expected_target);
        assert_ne!(type_match_calls[0].target, "new");
        assert!(
            !type_match_calls[0].target.ends_with("OtherType::new"),
            "dead_code nodes use bare symbol names, not scoped method names: {:#?}",
            type_match_calls[0]
        );
    }

    #[test]
    fn outbound_rows_carry_store_provenance_for_each_tier() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join("project");
        fs::create_dir_all(&root).expect("create project root");
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");
        let source = src_dir.join("lib.rs");
        fs::write(
            &source,
            r#"struct TypedTarget;
impl TypedTarget {
    fn typed_edge(&self) {}
}

struct NamedTarget;
impl NamedTarget {
    fn named_edge(&self) {}
}

fn run(typed: &TypedTarget) {
    local_target();
    typed.typed_edge();
    unknown.named_edge();
}

fn local_target() {}
"#,
        )
        .expect("write provenance fixture");

        let store = CallGraphStore::open(root.join(".store"), root.clone()).expect("open store");
        store
            .cold_build(std::slice::from_ref(&source))
            .expect("cold build provenance fixture");
        let snapshot = project_dead_code_snapshot(store.sqlite_path()).expect("project snapshot");

        assert_call_with_provenance(&snapshot, "local_target", PROVENANCE_TREESITTER);
        assert_call_with_provenance(&snapshot, "typed_edge", PROVENANCE_TYPE_MATCH);
        assert_call_with_provenance(&snapshot, "named_edge", PROVENANCE_NAME_MATCH);
    }

    fn assert_call_with_provenance(
        snapshot: &CallgraphSnapshot,
        target_fragment: &str,
        expected_provenance: &str,
    ) {
        assert!(
            snapshot.outbound_calls.iter().any(|call| {
                call.target.contains(target_fragment) && call.provenance == expected_provenance
            }),
            "expected projected call containing {target_fragment:?} with provenance \
             {expected_provenance:?}; calls: {:#?}",
            snapshot.outbound_calls
        );
    }
}
