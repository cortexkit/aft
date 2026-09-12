use crate::db::{SqliteStore, TrackedConnection};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[cfg(test)]
pub(crate) type ProjectionBeforeOpenObserver = dyn Fn(&Path) + Send + Sync + 'static;

#[cfg(test)]
thread_local! {
    static PROJECTION_BEFORE_OPEN_OBSERVER: std::cell::RefCell<Option<std::sync::Arc<ProjectionBeforeOpenObserver>>> =
        const { std::cell::RefCell::new(None) };
}

use crate::inspect::job::{CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot};
use crate::inspect::scanners::DEFAULT_EXPORT_MARKER_KIND;
use crate::symbols::SymbolKind;

use super::{
    database_ready, lang_from_label, path_identity_mismatch_reason, projection_write_revision,
    CallGraphStoreError, Result, BACKEND_TREESITTER, PROVENANCE_NAME_MATCH, PROVENANCE_TYPE_MATCH,
    PROVENANCE_VALUE_REF, TOP_LEVEL_SYMBOL,
};

#[cfg(test)]
pub(crate) fn set_projection_before_open_observer(
    observer: Option<std::sync::Arc<ProjectionBeforeOpenObserver>>,
) {
    PROJECTION_BEFORE_OPEN_OBSERVER.with(|slot| *slot.borrow_mut() = observer);
}

#[cfg(test)]
fn notify_projection_before_open_observer(db_path: &Path) {
    let observer = PROJECTION_BEFORE_OPEN_OBSERVER.with(|slot| slot.borrow().clone());
    if let Some(observer) = observer {
        observer(db_path);
    }
}

#[cfg(not(test))]
fn notify_projection_before_open_observer(_db_path: &Path) {}

pub fn project_dead_code_snapshot(db_path: &Path) -> Result<CallgraphSnapshot> {
    project_dead_code_snapshot_with_revision(db_path).map(|(_, snapshot)| snapshot)
}

/// Project the dead-code graph and read its durable write revision from one
/// read transaction so a cache identity always describes the returned snapshot.
pub(crate) fn project_dead_code_snapshot_with_revision(
    db_path: &Path,
) -> Result<(Option<u64>, CallgraphSnapshot)> {
    project_dead_code_snapshot_incremental(db_path, None)
}

pub(crate) fn project_dead_code_snapshot_incremental(
    db_path: &Path,
    previous: Option<(u64, &CallgraphSnapshot)>,
) -> Result<(Option<u64>, CallgraphSnapshot)> {
    if !db_path.is_file() {
        return Err(CallGraphStoreError::Unavailable(format!(
            "database does not exist: {}",
            db_path.display()
        )));
    }

    notify_projection_before_open_observer(db_path);
    let mut conn = TrackedConnection::open_path_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
        SqliteStore::CallgraphGeneration,
    )?;
    conn.busy_timeout(Duration::from_millis(5_000))?;
    let tx = conn.transaction()?;
    if !database_ready(&tx).unwrap_or(false) {
        return Err(CallGraphStoreError::Unavailable(
            "database is missing, stale, or mid-build".to_string(),
        ));
    }
    let write_revision = projection_write_revision(&tx)?;
    if let Some(reason) = path_identity_mismatch_reason(&tx)? {
        return Err(CallGraphStoreError::Unavailable(reason));
    }

    let project_root = project_root_from_backend_state(&tx)?;
    // Same predicate as InspectManager::callgraph_ready_for_snapshot: a store
    // that still has stale backend rows is not ready for dead_code, even if
    // the SQLite file opens and meta.ready=1.
    if stale_backend_file_count(&tx, &project_root)? > 0 {
        return Err(CallGraphStoreError::Unavailable(
            "callgraph has stale files pending refresh".to_string(),
        ));
    }
    let changed = match (previous, write_revision) {
        (Some((revision, _)), Some(current)) => projection_delta_since(&tx, revision, current)?,
        _ => None,
    };
    let mut paths = SnapshotPathResolver::new(&project_root);
    let (files, exported_symbols, outbound_calls, entry_point_symbols) =
        if let (Some(changed), Some((_, previous))) = (changed, previous) {
            let mut replacements = BTreeMap::new();
            let mut file_replacements = BTreeMap::new();
            let mut export_replacements = BTreeMap::new();
            let mut roots = previous.entry_point_symbols.clone();
            for file in &changed {
                let path = paths.resolve(file);
                file_replacements.insert(
                    path.clone(),
                    project_files_from_store(&tx, &mut paths, Some(file))?,
                );
                export_replacements.insert(
                    path.clone(),
                    exported_symbols_from_store(&tx, &mut paths, Some(file))?,
                );
                roots.remove(&path);
                roots.extend(entry_point_symbols_from_store(&tx, &mut paths, Some(file))?);
                replacements.insert(path, outbound_calls_for_file(&tx, &mut paths, file)?);
            }
            let calls = splice_files(&previous.outbound_calls, replacements, |call| {
                &call.caller_file
            });
            (
                splice_files(&previous.files, file_replacements, |path| path),
                splice_files(&previous.exported_symbols, export_replacements, |export| {
                    &export.file
                }),
                calls,
                roots,
            )
        } else {
            record_full_projection();
            (
                project_files_from_store(&tx, &mut paths, None)?,
                exported_symbols_from_store(&tx, &mut paths, None)?,
                outbound_calls_from_store(&tx, &mut paths)?,
                entry_point_symbols_from_store(&tx, &mut paths, None)?,
            )
        };
    let entry_points = entry_points_for_files(&project_root, &files);
    let snapshot = CallgraphSnapshot {
        generated_at: Some(SystemTime::now()),
        files,
        exported_symbols,
        outbound_calls,
        entry_points,
        entry_point_symbols,
    };
    tx.commit()?;

    Ok((write_revision, snapshot))
}

fn splice_files<T: Clone>(
    previous: &[T],
    replacements: BTreeMap<PathBuf, Vec<T>>,
    file: impl Fn(&T) -> &PathBuf,
) -> Vec<T> {
    let mut result = Vec::with_capacity(previous.len());
    let mut old = previous.iter().peekable();
    for (path, replacement) in replacements {
        while old.peek().is_some_and(|item| file(item) < &path) {
            result.push(old.next().expect("peeked item").clone());
        }
        while old.peek().is_some_and(|item| file(item) == &path) {
            old.next();
        }
        result.extend(replacement);
    }
    result.extend(old.cloned());
    result
}

// A bounded durable journal lets a reader bridge multiple watcher transactions.
// Missing entries (including writes by older binaries) always force a cold read.
const DELTA_HISTORY: u64 = 64;
const MAX_DELTA_BYTES: usize = 256 * 1024;

pub(super) fn extend_projection_dependents(
    conn: &Connection,
    file: &str,
    callers: &mut BTreeSet<String>,
) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT caller_file FROM refs WHERE target_file = ?1
         UNION SELECT r.caller_file FROM edges e JOIN refs r ON r.ref_id = e.ref_id WHERE e.target_file = ?1
         UNION SELECT file_path FROM file_dependencies WHERE dep_file = ?1",
    )?;
    for row in statement.query_map([file], |row| row.get::<_, String>(0))? {
        callers.insert(row?);
    }
    Ok(())
}

pub(super) fn record_projection_delta(
    tx: &Transaction<'_>,
    callers: &BTreeSet<String>,
) -> Result<()> {
    super::bump_projection_write_revision(tx)?;
    let revision = projection_write_revision(tx)?.expect("just advanced revision");
    let key = format!("projection_delta_{}", revision % DELTA_HISTORY);
    let value = serde_json::to_string(&(revision, callers))
        .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
    if value.len() <= MAX_DELTA_BYTES {
        tx.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES(?1, ?2)",
            params![key, value],
        )?;
    } else {
        // Oversized batches need no retained history: a cold projection is safer
        // than allowing per-root journal storage to grow with the corpus.
        tx.execute("DELETE FROM meta WHERE k = ?1", [key])?;
    }
    Ok(())
}

fn projection_delta_since(
    conn: &Connection,
    previous: u64,
    current: u64,
) -> Result<Option<BTreeSet<String>>> {
    if current < previous || current - previous > DELTA_HISTORY {
        return Ok(None);
    }
    let mut callers = BTreeSet::new();
    for revision in previous.saturating_add(1)..=current {
        let value: Option<String> = conn
            .query_row(
                "SELECT v FROM meta WHERE k = ?1",
                [format!("projection_delta_{}", revision % DELTA_HISTORY)],
                |row| row.get(0),
            )
            .optional()?;
        let Some(value) = value else { return Ok(None) };
        let Ok((stored, files)) = serde_json::from_str::<(u64, BTreeSet<String>)>(&value) else {
            return Ok(None);
        };
        if stored != revision {
            return Ok(None);
        }
        callers.extend(files);
    }
    Ok(Some(callers))
}

#[cfg(test)]
thread_local! {
    static PROJECTION_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

#[cfg(test)]
pub(crate) fn take_projection_work() -> (usize, usize) {
    PROJECTION_WORK.with(|work| work.replace((0, 0)))
}

fn record_outbound_rows(rows: usize) {
    #[cfg(test)]
    PROJECTION_WORK.with(|work| {
        let (full, read) = work.get();
        work.set((full, read + rows));
    });
    #[cfg(not(test))]
    let _ = rows;
}

fn record_full_projection() {
    #[cfg(test)]
    PROJECTION_WORK.with(|work| {
        let (full, read) = work.get();
        work.set((full + 1, read));
    });
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
    file: Option<&str>,
) -> Result<Vec<PathBuf>> {
    let mut statement = conn.prepare(if file.is_some() {
        "SELECT path FROM files WHERE path = ?1 ORDER BY path"
    } else {
        "SELECT path FROM files ORDER BY path"
    })?;
    let files = statement
        .query_map(rusqlite::params_from_iter(file), |row| {
            row.get::<_, String>(0)
        })?
        .map(|path| path.map(|path| paths.resolve(&path)))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(files)
}

fn exported_symbols_from_store(
    conn: &Connection,
    paths: &mut SnapshotPathResolver<'_>,
    file: Option<&str>,
) -> Result<Vec<CallgraphExport>> {
    let filter = if file.is_some() {
        " AND file_path = ?1"
    } else {
        ""
    };
    let mut statement = conn.prepare(&format!(
        "SELECT file_path, name, kind, start_line, exported, is_default_export
         FROM nodes
         WHERE (exported != 0 OR is_default_export != 0){filter}
         ORDER BY file_path, start_line, name, kind, id",
    ))?;
    let rows = statement.query_map(rusqlite::params_from_iter(file), |row| {
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
    file: Option<&str>,
) -> Result<BTreeMap<PathBuf, BTreeSet<String>>> {
    let filter = if file.is_some() {
        " AND n.file_path = ?1"
    } else {
        ""
    };
    let mut statement = conn.prepare(&format!(
        "SELECT n.file_path, n.name, n.scoped_name, n.kind, n.exported, f.lang
         FROM nodes n
         JOIN files f ON f.path = n.file_path
         WHERE n.is_callgraph_entry_point != 0{filter}
         ORDER BY n.file_path, n.start_line, n.name, n.kind, n.id",
    ))?;
    let rows = statement.query_map(rusqlite::params_from_iter(file), |row| {
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

const OUTBOUND_CALLS_SQL: &str = "SELECT r.caller_file,
            r.caller_node,
            n.name,
            r.short_name,
            r.full_ref,
            r.status,
            COALESCE(r.target_file, e.target_file),
            COALESCE(tn.name, r.target_symbol, e.target_symbol),
            r.line,
            COALESCE(e.provenance, r.provenance),
            r.byte_start,
            r.byte_end,
            r.ref_id
     FROM refs r
     LEFT JOIN nodes n ON n.id = r.caller_node
     LEFT JOIN edges e ON e.ref_id = r.ref_id AND e.kind = r.kind
     LEFT JOIN nodes tn ON tn.id = e.target_node
     WHERE r.kind IN ('call', 'value_ref')";

fn outbound_calls_from_store(
    conn: &Connection,
    paths: &mut SnapshotPathResolver<'_>,
) -> Result<Vec<CallgraphOutboundCall>> {
    outbound_calls_query(conn, paths, None)
}

fn outbound_calls_for_file(
    conn: &Connection,
    paths: &mut SnapshotPathResolver<'_>,
    file: &str,
) -> Result<Vec<CallgraphOutboundCall>> {
    outbound_calls_query(conn, paths, Some(file))
}

fn outbound_calls_query(
    conn: &Connection,
    paths: &mut SnapshotPathResolver<'_>,
    file: Option<&str>,
) -> Result<Vec<CallgraphOutboundCall>> {
    let sql = match file {
        Some(_) => format!("{OUTBOUND_CALLS_SQL} AND r.caller_file = ?1"),
        None => OUTBOUND_CALLS_SQL.to_owned(),
    };
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(file), |row| {
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
            byte_start: row.get(10)?,
            byte_end: row.get(11)?,
            ref_id: row.get(12)?,
        })
    })?;
    let mut rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    record_outbound_rows(rows.len());
    // SQLite otherwise materializes this 300k+-row ordering in a temporary
    // B-tree. Sorting the already-required projection rows in memory preserves
    // its BINARY/NULL-first order without turning a read-only snapshot into
    // roughly 100 MB of physical temporary-file writes.
    rows.sort_by(|left, right| {
        left.caller_file
            .cmp(&right.caller_file)
            .then_with(|| left.caller_symbol.cmp(&right.caller_symbol))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.byte_start.cmp(&right.byte_start))
            .then_with(|| left.byte_end.cmp(&right.byte_end))
            .then_with(|| left.ref_id.cmp(&right.ref_id))
    });

    let mut calls = Vec::with_capacity(rows.len());
    let mut stale_caller_nodes = 0usize;
    for row in rows {
        if row.provenance == PROVENANCE_VALUE_REF
            && !matches!(row.status.as_str(), "resolved" | "resolved_local")
        {
            continue;
        }
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
                || provenance == PROVENANCE_VALUE_REF
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
    // Never let the two branches disagree on Windows verbatim form: bare
    // fs::canonicalize returns \?\-prefixed paths while the fallback is
    // clean, so snapshot path spellings depended on whether canonicalize
    // succeeded — and every downstream join against normalized paths broke
    // only when it did. One canonical (verbatim-stripped) form, both branches.
    crate::inspect::job::canonicalize_normalized(path)
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
        "kernel" => Some(SymbolKind::Kernel),
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
    byte_start: usize,
    byte_end: usize,
    ref_id: String,
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

        let stats = store
            .refresh_files(std::slice::from_ref(&source))
            .expect("refresh unchanged stale file");
        assert_eq!(stats.refreshed_own_files, 0);
        assert_eq!(stats.changed_files, vec!["src/lib.ts".to_string()]);
        let snapshot = project_dead_code_snapshot(store.sqlite_path()).expect(
            "refresh of unchanged files must clear leftover stale rows so projection can proceed",
        );
        assert_eq!(snapshot.files.len(), 1);
        assert!(store.stale_files().expect("read stale files").is_empty());
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
        // Expectations mirror the projection's normalized (verbatim-stripped)
        // canonical form.
        let expected_target = format!(
            "{}::new",
            crate::inspect::job::canonicalize_normalized(&target_source).display()
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
    fn outbound_projection_sorts_in_rust_without_a_sqlite_temp_btree() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join("project");
        fs::create_dir_all(&root).expect("create project root");
        let source = root.join("main.ts");
        fs::write(
            &source,
            r#"topLevel();
export function zed() { second(); first(); }
export function alpha() { third(); }
"#,
        )
        .expect("write ordering fixture");
        let store = CallGraphStore::open(root.join(".store"), root).expect("open store");
        store
            .cold_build(std::slice::from_ref(&source))
            .expect("cold build ordering fixture");

        let conn = Connection::open(store.sqlite_path()).expect("open projection fixture");
        let mut plan = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {OUTBOUND_CALLS_SQL}"))
            .expect("prepare outbound query plan");
        let details = plan
            .query_map([], |row| row.get::<_, String>(3))
            .expect("query outbound plan")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect outbound plan");
        assert!(
            details
                .iter()
                .all(|detail| !detail.contains("USE TEMP B-TREE")),
            "outbound projection must not spill its ordering to disk: {details:?}"
        );

        let snapshot = project_dead_code_snapshot(store.sqlite_path()).expect("project snapshot");
        let order = snapshot
            .outbound_calls
            .iter()
            .map(|call| (call.caller_symbol.as_str(), call.target.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                (TOP_LEVEL_SYMBOL, "topLevel"),
                ("alpha", "third"),
                ("zed", "second"),
                ("zed", "first"),
            ]
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
    let _ = callback_target;
    typed.typed_edge();
    unknown.named_edge();
}

fn local_target() {}
fn callback_target() {}
"#,
        )
        .expect("write provenance fixture");

        let store = CallGraphStore::open(root.join(".store"), root.clone()).expect("open store");
        store
            .cold_build(std::slice::from_ref(&source))
            .expect("cold build provenance fixture");
        let snapshot = project_dead_code_snapshot(store.sqlite_path()).expect("project snapshot");

        assert_call_with_provenance(&snapshot, "local_target", PROVENANCE_TREESITTER);
        assert_call_with_provenance(&snapshot, "callback_target", PROVENANCE_VALUE_REF);
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
