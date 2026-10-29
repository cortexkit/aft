//! Persist ruled method edges and unknown-receiver liveness in the same
//! transaction as ordinary bindings. The input is immutable parse blobs only.
//!
//! Every file's resolver inputs (its [`FileFacts`]) and method-call sites are
//! kept in `view_dispatch_files`, and the keys each file's resolutions read in
//! `view_dispatch_dependencies`. An incremental build then decodes only the
//! changed files' blobs, resolves only the sites whose recorded keys name a
//! changed input, and leaves every other file's rows as the base generation
//! wrote them. The result equals a cold build of the same manifest.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use crate::callgraph_store::join::{
    self,
    dispatch::{
        dependency, DispatchFacts, FieldHint, MemoizedResolver, MethodHint, Resolution, Resolver,
        ReturnHint, SiteHint, Target, TypeHint,
    },
    BlobImport, ManifestBlobReader, ParseBlob,
};
use crate::callgraph_store::{CallGraphStoreError, Result};
use crate::views::{Manifest, ManifestEntry};

/// What the method-call pass may assume about this transaction.
pub(super) struct EmitScope<'a> {
    /// Manifest paths whose `nodes` rows this transaction rewrote, or `None`
    /// for a cold build, where every path counts as rewritten. Node IDs encode
    /// the AST ordinal, so edges touching these files must look nodes up again.
    pub(super) changed_paths: Option<&'a BTreeSet<Vec<u8>>>,
    /// Ref IDs for which ordinary emission wrote an edge in this transaction.
    /// An ordinary edge on a method-call site's ref ID can only come from this
    /// set: the method-call pass of every earlier build deleted such edges.
    pub(super) ordinary_edge_refs: &'a HashSet<String>,
    /// Caller files whose `refs` rows this transaction wrote. Their site
    /// statuses must be written again and their import targets read again.
    pub(super) rewritten_ref_files: &'a BTreeSet<String>,
    /// Receiver type targets the join resolved in this transaction. With
    /// `reuse` false it must hold every Rust file of the manifest.
    pub(super) type_targets: &'a BTreeMap<String, join::DispatchTypeTargets>,
    /// Whether the base generation's dispatch inputs and rows may be reused.
    /// When false, every blob is decoded and every site resolved.
    pub(super) reuse: bool,
}

/// Work done by one method-call pass, for tests and profiling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DispatchWork {
    /// Parse blobs decoded by this pass.
    pub(crate) decoded_blobs: usize,
    /// Files whose sites were resolved and whose rows were reconciled.
    pub(crate) resolved_files: usize,
    /// Method-call sites resolved.
    pub(crate) resolved_sites: usize,
    /// Every file was resolved, as in a cold build.
    pub(crate) full: bool,
}

#[cfg(test)]
thread_local! {
    /// Lets a test or measurement write every site row by row, as the pass did
    /// before it wrote only differences, to compare the two databases.
    pub(crate) static WRITE_ROW_BY_ROW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Makes an incremental pass resolve and rewrite every file, as if nothing
    /// could be carried over from the base generation. Tests use it to show
    /// that the work counters observe the carry-over.
    pub(crate) static RESOLVE_EVERY_FILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The import fields method-call resolution reads.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct ImportFact {
    module_path: String,
    names: Vec<String>,
    default_import: Option<String>,
}

/// Everything method-call resolution reads about one file, apart from its
/// call sites. Positions are left out, so an edit that only moves code keeps
/// a file's facts, and the resolutions that read them, unchanged.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct FileFacts {
    language: String,
    types: Vec<TypeHint>,
    methods: Vec<MethodHint>,
    fields: Vec<FieldHint>,
    returns: Vec<ReturnHint>,
    imports: Vec<ImportFact>,
    default_export_symbol: Option<String>,
    /// Target file of each resolved import, by module path.
    import_targets: BTreeMap<String, String>,
    /// Target file and symbol of each Rust receiver type name.
    type_targets: BTreeMap<String, (String, String)>,
}

impl FileFacts {
    fn new(parse: &ParseBlob) -> Self {
        Self {
            language: parse.language.clone(),
            types: parse.dispatch.types.clone(),
            methods: parse.dispatch.methods.clone(),
            fields: parse.dispatch.fields.clone(),
            returns: parse.dispatch.returns.clone(),
            imports: parse
                .imports
                .iter()
                .map(|import| ImportFact {
                    module_path: import.module_path.clone(),
                    names: import.names.clone(),
                    default_import: import.default_import.clone(),
                })
                .collect(),
            default_export_symbol: parse.default_export_symbol.clone(),
            import_targets: BTreeMap::new(),
            type_targets: BTreeMap::new(),
        }
    }

    /// A parse blob carrying only what [`Resolver`] reads. Fields the resolver
    /// never reads (symbols, references, AST nodes, import positions, sites)
    /// are left empty.
    fn resolver_parse(&self) -> ParseBlob {
        ParseBlob {
            entry_point_symbols: Vec::new(),
            dispatch: DispatchFacts {
                types: self.types.clone(),
                methods: self.methods.clone(),
                sites: Vec::new(),
                fields: self.fields.clone(),
                returns: self.returns.clone(),
            },
            extractor_version: String::new(),
            language: self.language.clone(),
            ast_nodes: Vec::new(),
            symbols: Vec::new(),
            default_export_symbol: self.default_export_symbol.clone(),
            exported_symbols: Vec::new(),
            callable_symbols: Vec::new(),
            python_bindings: Vec::new(),
            imports: self
                .imports
                .iter()
                .map(|import| BlobImport {
                    ordinal: 0,
                    module_path: import.module_path.clone(),
                    names: import.names.clone(),
                    default_import: import.default_import.clone(),
                    namespace_import: None,
                    byte_start: 0,
                    byte_end: 0,
                    raw_text: String::new(),
                    type_only: false,
                    side_effect: false,
                })
                .collect(),
            refs: Vec::new(),
        }
    }

    /// The keys of every resolution that may read this file's facts, when the
    /// facts change from `old` to `new` (either side may be absent).
    fn changed_keys(
        path: &str,
        old: Option<&Self>,
        new: Option<&Self>,
        keys: &mut BTreeSet<String>,
    ) {
        if old == new {
            return;
        }
        keys.insert(dependency::file(path));
        keys.insert(dependency::directory(path));
        for facts in old.into_iter().chain(new) {
            // A resolution that found this file through a method name recorded
            // only that name, so every method name of the file is affected.
            keys.extend(
                facts
                    .methods
                    .iter()
                    .map(|method| dependency::method(&method.name)),
            );
            if facts.language == "go" {
                keys.insert(dependency::GO_INTERFACES.to_string());
            }
        }
    }
}

fn json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| CallGraphStoreError::Unavailable(e.to_string()))
}

fn from_json<T: for<'de> Deserialize<'de>>(payload: &str) -> Result<T> {
    serde_json::from_str(payload).map_err(|e| CallGraphStoreError::Unavailable(e.to_string()))
}

fn digest(payload: &str) -> String {
    blake3::hash(payload.as_bytes()).to_hex().to_string()
}

/// Dependency keys of a file's receiver type target trace; they share the
/// dependency table with resolution keys under their own prefix.
fn type_target_key(dependency: &str) -> String {
    format!("t:{dependency}")
}

const CREATE_TABLES: &str = "CREATE TABLE IF NOT EXISTS view_dispatch_sites (
        caller_file TEXT NOT NULL, ordinal INTEGER NOT NULL, language TEXT NOT NULL,
        unresolved_receiver_sites INTEGER NOT NULL, dynamic_member_sites INTEGER NOT NULL,
        external INTEGER NOT NULL, PRIMARY KEY(caller_file, ordinal));
    CREATE TABLE IF NOT EXISTS view_unknown_live (
        caller_file TEXT NOT NULL, ordinal INTEGER NOT NULL, target_file TEXT NOT NULL,
        target_symbol TEXT NOT NULL, PRIMARY KEY(caller_file, ordinal, target_file, target_symbol));
    CREATE TABLE IF NOT EXISTS view_dispatch_files (
        file_path TEXT PRIMARY KEY, facts_digest TEXT NOT NULL, sites_digest TEXT NOT NULL,
        facts TEXT NOT NULL, type_target_trace TEXT, sites TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS view_dispatch_dependencies (
        file_path TEXT NOT NULL, dependency TEXT NOT NULL,
        PRIMARY KEY(file_path, dependency)) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS idx_view_dispatch_dependencies_dependency
        ON view_dispatch_dependencies(dependency);";

/// True when the database holds the per-file dispatch inputs an incremental
/// pass reuses. Generations written before they existed are rebuilt once.
pub(super) fn reusable(transaction: &Transaction<'_>) -> Result<bool> {
    Ok(transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='view_dispatch_files')",
        [],
        |row| row.get(0),
    )?)
}

/// Reads, from `view_dispatch_files`, the receiver type target trace of each
/// file whose stored trace dependencies include one of `seeds`.
pub(super) fn type_target_traces(
    transaction: &Transaction<'_>,
    seeds: &BTreeSet<String>,
) -> Result<BTreeMap<String, join::DispatchTypeTargetTrace>> {
    let mut files = BTreeSet::new();
    {
        let mut dependents = transaction.prepare_cached(
            "SELECT file_path FROM view_dispatch_dependencies
             INDEXED BY idx_view_dispatch_dependencies_dependency WHERE dependency=?1",
        )?;
        for seed in seeds {
            for file in
                dependents.query_map([type_target_key(seed)], |row| row.get::<_, String>(0))?
            {
                files.insert(file?);
            }
        }
    }
    let mut read = transaction
        .prepare_cached("SELECT type_target_trace FROM view_dispatch_files WHERE file_path=?1")?;
    let mut traces = BTreeMap::new();
    for file in files {
        let trace: Option<Option<String>> = read.query_row([&file], |row| row.get(0)).optional()?;
        if let Some(Some(trace)) = trace {
            traces.insert(file, from_json(&trace)?);
        }
    }
    Ok(traces)
}

/// One file's row of `view_dispatch_files`, as stored or as it must be stored.
struct FileRow {
    facts: FileFacts,
    facts_json: String,
    facts_digest: String,
    sites_digest: String,
    /// The file's sites, once read from the stored row or the decoded blob.
    sites: Option<Vec<SiteHint>>,
    /// Serialized sites of a decoded blob, whose whole row is written.
    sites_json: Option<String>,
    /// A trace to store; `None` keeps the stored one.
    trace: Option<join::DispatchTypeTargetTrace>,
    /// The stored facts or trace differ from this row's.
    dirty: bool,
}

impl FileRow {
    fn decoded(parse: &ParseBlob, facts: FileFacts) -> Result<Self> {
        let facts_json = json(&facts)?;
        let sites_json = json(&parse.dispatch.sites)?;
        Ok(Self {
            facts_digest: digest(&facts_json),
            facts,
            facts_json,
            sites_digest: digest(&sites_json),
            sites: Some(parse.dispatch.sites.clone()),
            sites_json: Some(sites_json),
            trace: None,
            dirty: true,
        })
    }

    fn set_facts(&mut self, facts: FileFacts) -> Result<()> {
        if facts != self.facts {
            self.facts_json = json(&facts)?;
            self.facts_digest = digest(&self.facts_json);
            self.facts = facts;
            self.dirty = true;
        }
        Ok(())
    }
}

/// Import targets of the given caller files (every caller when `None`), read
/// from this transaction's `refs` rows. A later row of one module replaces an
/// earlier one, in the order a cold build reads them.
fn import_targets(
    transaction: &Transaction<'_>,
    files: Option<&BTreeSet<String>>,
) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    let mut targets = BTreeMap::<String, BTreeMap<String, String>>::new();
    let read = |r: &rusqlite::Row<'_>| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    };
    let mut rows = Vec::new();
    match files {
        None => {
            let mut query = transaction.prepare("SELECT caller_file, module_path, target_file FROM refs WHERE kind='import' AND module_path IS NOT NULL AND target_file IS NOT NULL ORDER BY caller_file, module_path, target_file")?;
            for row in query.query_map([], read)? {
                rows.push(row?);
            }
        }
        Some(files) => {
            let mut query = transaction.prepare("SELECT caller_file, module_path, target_file FROM refs INDEXED BY idx_refs_kind_caller_file WHERE kind='import' AND caller_file=?1 AND module_path IS NOT NULL AND target_file IS NOT NULL ORDER BY module_path, target_file")?;
            for file in files {
                for row in query.query_map([file], read)? {
                    rows.push(row?);
                }
            }
        }
    }
    for (caller, module, target) in rows {
        targets.entry(caller).or_default().insert(module, target);
    }
    Ok(targets)
}

/// Files with a method-call edge into one of `targets`. Those edges carry the
/// node ID of their target, which a rewritten file may have renumbered.
fn files_with_edges_into(
    transaction: &Transaction<'_>,
    targets: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut query = transaction.prepare(
        "SELECT ref_id FROM edges INDEXED BY idx_edges_target_file_symbol
         WHERE target_file=?1 AND provenance IN ('exact', 'dispatch', 'name_match')",
    )?;
    let mut files = BTreeSet::new();
    for target in targets {
        for ref_id in query.query_map([target], |row| row.get::<_, String>(0))? {
            if let Some((file, _)) = ref_id?
                .strip_prefix("view:")
                .and_then(|rest| rest.rsplit_once(':'))
            {
                files.insert(file.to_string());
            }
        }
    }
    Ok(files)
}

/// Files whose stored dependency keys include one of `keys`.
fn dependents(transaction: &Transaction<'_>, keys: &BTreeSet<String>) -> Result<BTreeSet<String>> {
    let mut query = transaction.prepare(
        "SELECT file_path FROM view_dispatch_dependencies
         INDEXED BY idx_view_dispatch_dependencies_dependency WHERE dependency=?1",
    )?;
    let mut files = BTreeSet::new();
    for key in keys {
        for file in query.query_map([key], |row| row.get::<_, String>(0))? {
            files.insert(file?);
        }
    }
    Ok(files)
}

/// Reads the stored sites of the listed files that do not have them yet.
fn load_sites<'p>(
    transaction: &Transaction<'_>,
    rows: &mut BTreeMap<String, FileRow>,
    files: impl IntoIterator<Item = &'p String>,
) -> Result<()> {
    let mut query =
        transaction.prepare("SELECT sites FROM view_dispatch_files WHERE file_path=?1")?;
    for file in files {
        let Some(row) = rows.get_mut(file).filter(|row| row.sites.is_none()) else {
            continue;
        };
        let sites: String = query.query_row([file], |row| row.get(0))?;
        row.sites = Some(from_json(&sites)?);
    }
    Ok(())
}

pub(super) fn emit(
    transaction: &Transaction<'_>,
    manifest: &Manifest,
    reader: &impl ManifestBlobReader,
    scope: &EmitScope<'_>,
) -> Result<DispatchWork> {
    // With `AFT_VIEW_PROFILE` set, each stage of this pass prints its wall
    // time to stderr; without it nothing is timed or printed.
    let mut stage = super::profile::StageTimer::new("dispatch");
    transaction.execute_batch(CREATE_TABLES)?;
    let changed = scope.changed_paths.map(|paths| {
        paths
            .iter()
            .filter_map(|path| String::from_utf8(path.clone()).ok())
            .collect::<BTreeSet<_>>()
    });
    let incremental = scope.reuse && changed.is_some();
    let changed = changed.unwrap_or_default();
    let mut work = DispatchWork {
        full: !incremental,
        ..DispatchWork::default()
    };
    let mut keys = Vec::new();
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
        keys.push((path, key.as_str()));
    }

    // The base generation's rows, without their sites, and the facts of every
    // file whose facts this pass may change.
    let mut rows = BTreeMap::<String, FileRow>::new();
    let mut touched = BTreeSet::new();
    if incremental {
        let mut query = transaction.prepare(
            "SELECT file_path, facts_digest, sites_digest, facts FROM view_dispatch_files",
        )?;
        let stored = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (path, facts_digest, sites_digest, facts_json) in stored {
            if changed.contains(&path) {
                touched.insert(path.clone());
            }
            rows.insert(
                path,
                FileRow {
                    facts: from_json(&facts_json)?,
                    facts_json,
                    facts_digest,
                    sites_digest,
                    sites: None,
                    sites_json: None,
                    trace: None,
                    dirty: false,
                },
            );
        }
        touched.extend(changed.iter().cloned());
        touched.extend(scope.rewritten_ref_files.iter().cloned());
        touched.extend(scope.type_targets.keys().cloned());
    }
    let old_facts = touched
        .iter()
        .filter_map(|path| rows.get(path).map(|row| (path.clone(), row.facts.clone())))
        .collect::<BTreeMap<_, _>>();
    for path in &changed {
        if incremental {
            rows.remove(path);
        }
    }
    stage.finish("load_stored_facts", format_args!("files={}", rows.len()));

    // Decode every blob for a full pass, otherwise only the changed files'.
    let decode = keys
        .iter()
        .filter(|(path, _)| !incremental || changed.contains(*path))
        .copied()
        .collect::<Vec<_>>();
    let blobs = reader
        .read_callgraph_blobs_decoded(&decode.iter().map(|(_, key)| *key).collect::<Vec<_>>())
        .map_err(|e| CallGraphStoreError::Unavailable(e.to_string()))?;
    work.decoded_blobs = decode.len();
    stage.finish("decode_blobs", format_args!("files={}", decode.len()));
    let import_files = incremental.then(|| {
        changed
            .iter()
            .chain(scope.rewritten_ref_files)
            .cloned()
            .collect::<BTreeSet<_>>()
    });
    let mut imports = import_targets(transaction, import_files.as_ref())?;
    for ((path, key), blob) in decode.iter().zip(blobs) {
        let blob = blob.ok_or_else(|| {
            CallGraphStoreError::Unavailable(format!("missing dispatch blob {key}"))
        })?;
        let Some(parse) = blob.parse() else {
            continue;
        };
        let mut facts = FileFacts::new(parse);
        facts.import_targets = imports.remove(*path).unwrap_or_default();
        let mut row = FileRow::decoded(parse, facts)?;
        if let Some(targets) = scope.type_targets.get(*path) {
            let mut facts = row.facts.clone();
            facts.type_targets = targets.targets.clone();
            row.set_facts(facts)?;
            row.trace = Some(targets.trace.clone());
        }
        rows.insert((*path).to_string(), row);
    }
    if incremental {
        // An unchanged file keeps its stored facts, except for the import
        // targets of its rewritten references and the type targets the join
        // resolved again.
        for path in scope.rewritten_ref_files {
            if let Some(row) = rows.get_mut(path).filter(|row| row.sites_json.is_none()) {
                let mut facts = row.facts.clone();
                facts.import_targets = imports.remove(path).unwrap_or_default();
                row.set_facts(facts)?;
            }
        }
        for (path, targets) in scope.type_targets {
            if let Some(row) = rows.get_mut(path).filter(|row| row.sites_json.is_none()) {
                let mut facts = row.facts.clone();
                facts.type_targets = targets.targets.clone();
                row.set_facts(facts)?;
                row.trace = Some(targets.trace.clone());
            }
        }
    }
    stage.finish(
        "import_and_type_targets",
        format_args!("imports={}", import_files.as_ref().map_or(0, BTreeSet::len)),
    );

    // The files whose sites are resolved and whose rows are reconciled.
    let mut selected = if incremental {
        let mut dirty = BTreeSet::new();
        for path in &touched {
            FileFacts::changed_keys(
                path,
                old_facts.get(path),
                rows.get(path).map(|row| &row.facts),
                &mut dirty,
            );
        }
        let mut selected = changed.clone();
        selected.extend(scope.rewritten_ref_files.iter().cloned());
        selected.extend(files_with_edges_into(transaction, &changed)?);
        let dependent = dependents(transaction, &dirty)?;
        stage.finish(
            "select",
            format_args!("dirty_keys={} dependents={}", dirty.len(), dependent.len()),
        );
        selected.extend(dependent);
        #[cfg(test)]
        if RESOLVE_EVERY_FILE.with(std::cell::Cell::get) {
            selected.extend(rows.keys().cloned());
        }
        selected
    } else {
        rows.keys().cloned().collect()
    };
    load_sites(transaction, &mut rows, &selected)?;
    let duplicate_ordinal = |rows: &BTreeMap<String, FileRow>, files: &BTreeSet<String>| {
        files.iter().filter_map(|file| rows.get(file)).any(|row| {
            let sites = row.sites.as_deref().unwrap_or_default();
            let ordinals = sites
                .iter()
                .map(|site| site.ordinal)
                .collect::<HashSet<_>>();
            ordinals.len() != sites.len()
        })
    };
    let mut duplicate = duplicate_ordinal(&rows, &selected);
    #[cfg(test)]
    let row_by_row = WRITE_ROW_BY_ROW.with(std::cell::Cell::get);
    #[cfg(not(test))]
    let row_by_row = false;
    if incremental && (duplicate || row_by_row) {
        // Only the original row-by-row order over every file defines the
        // result of two sites sharing an ordinal (including its constraint
        // error), so the whole manifest is written as a cold build would.
        selected.extend(rows.keys().cloned());
        let every = rows.keys().cloned().collect::<Vec<_>>();
        load_sites(transaction, &mut rows, &every)?;
        duplicate = duplicate_ordinal(&rows, &selected);
    }
    let whole = !incremental || rows.keys().all(|path| selected.contains(path));
    stage.finish("load_sites", format_args!("selected={}", selected.len()));

    let parses = rows
        .iter()
        .map(|(path, row)| (path.clone(), row.facts.resolver_parse()))
        .collect::<BTreeMap<_, _>>();
    let mut resolver = Resolver::new(
        parses
            .iter()
            .map(|(path, parse)| (path.clone(), parse))
            .collect(),
    );
    for (path, row) in &rows {
        for (module, target) in &row.facts.import_targets {
            resolver
                .import_targets
                .insert((path.clone(), module.clone()), target.clone());
        }
        for (name, target) in &row.facts.type_targets {
            resolver
                .type_targets
                .insert((path.clone(), name.clone()), target.clone());
        }
    }
    stage.finish("resolver_index", format_args!("files={}", parses.len()));
    let memoized = MemoizedResolver::new(&resolver);
    let mut sites = Vec::new();
    let mut file_dependencies = BTreeMap::<String, BTreeSet<String>>::new();
    resolver.take_dependencies();
    for path in &selected {
        let Some(row) = rows.get(path) else {
            continue;
        };
        for site in row.sites.as_deref().unwrap_or_default() {
            sites.push(ResolvedSite {
                file: path,
                language: &row.facts.language,
                ref_id: format!("view:{path}:{}", site.ordinal),
                site,
                resolution: memoized.resolve(path, site),
            });
        }
        // Every file depends on its own facts, which `Resolver::resolve`
        // reads without recording.
        let mut dependencies = resolver.take_dependencies();
        dependencies.insert(dependency::file(path));
        file_dependencies.insert(path.clone(), dependencies);
    }
    work.resolved_files = file_dependencies.len();
    work.resolved_sites = sites.len();
    let targets = sites
        .iter()
        .map(|site| site.resolution.targets.len())
        .sum::<usize>();
    stage.finish(
        "resolve_sites",
        format_args!(
            "files={} sites={} targets={targets}",
            work.resolved_files,
            sites.len()
        ),
    );

    // The signature is persisted metadata that cold and incremental builds
    // must agree on. The row-by-row writer also uses it to decide whether the
    // site tables must be rebuilt.
    let mut signature = blake3::Hasher::new();
    for (path, row) in &rows {
        for part in [path, &row.facts_digest, &row.sites_digest] {
            signature.update(part.as_bytes());
            signature.update(b"\0");
        }
    }
    let signature = signature.finalize().to_hex().to_string();
    let old: Option<String> = transaction
        .query_row(
            "SELECT v FROM meta WHERE k='view_dispatch_signature'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let signature_changed = old.as_deref() != Some(&signature);
    if duplicate || row_by_row {
        debug_assert!(whole);
        // Two sites of one file sharing an ordinal share one ref ID and one
        // `view_dispatch_sites` key, so only the original row-by-row order
        // defines the result (including its constraint error).
        write_row_by_row(transaction, &sites, signature_changed)?;
        stage.finish("write_row_by_row", format_args!(""));
    } else {
        let files = (!whole).then_some(&selected);
        let counts = write_differences(transaction, &sites, scope, files)?;
        stage.finish("write_differences", format_args!("{counts:?}"));
    }
    drop(sites);
    drop(memoized);
    drop(resolver);
    transaction.execute(
        "INSERT OR REPLACE INTO meta(k,v) VALUES ('view_dispatch_signature',?1)",
        [signature],
    )?;
    persist(
        transaction,
        incremental,
        &changed,
        &rows,
        &file_dependencies,
    )?;
    stage.finish("persist_inputs", format_args!(""));
    Ok(work)
}

/// Brings `view_dispatch_files` and `view_dispatch_dependencies` to the rows
/// a cold build writes: one row per file with facts, and per file the keys
/// its resolutions recorded plus the keys of its type target trace.
fn persist(
    transaction: &Transaction<'_>,
    incremental: bool,
    changed: &BTreeSet<String>,
    rows: &BTreeMap<String, FileRow>,
    file_dependencies: &BTreeMap<String, BTreeSet<String>>,
) -> Result<()> {
    let trace_keys = |trace: &join::DispatchTypeTargetTrace| {
        trace
            .dependencies()
            .iter()
            .map(|dependency| type_target_key(dependency))
            .collect::<BTreeSet<_>>()
    };
    if !incremental {
        transaction.execute_batch(
            "DELETE FROM view_dispatch_files; DELETE FROM view_dispatch_dependencies;",
        )?;
    } else {
        let mut delete_file =
            transaction.prepare("DELETE FROM view_dispatch_files WHERE file_path=?1")?;
        let mut delete_dependencies =
            transaction.prepare("DELETE FROM view_dispatch_dependencies WHERE file_path=?1")?;
        for path in changed.iter().filter(|path| !rows.contains_key(*path)) {
            delete_file.execute([path])?;
            delete_dependencies.execute([path])?;
        }
    }
    let mut replace = transaction.prepare(
        "INSERT OR REPLACE INTO view_dispatch_files
         (file_path, facts_digest, sites_digest, facts, type_target_trace, sites)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut update_facts = transaction
        .prepare("UPDATE view_dispatch_files SET facts_digest=?2, facts=?3 WHERE file_path=?1")?;
    let mut update_trace = transaction
        .prepare("UPDATE view_dispatch_files SET type_target_trace=?2 WHERE file_path=?1")?;
    let mut read_dependencies = transaction
        .prepare("SELECT dependency FROM view_dispatch_dependencies WHERE file_path=?1")?;
    let mut insert_dependency = transaction
        .prepare("INSERT INTO view_dispatch_dependencies(file_path, dependency) VALUES (?1, ?2)")?;
    let mut delete_dependency = transaction
        .prepare("DELETE FROM view_dispatch_dependencies WHERE file_path=?1 AND dependency=?2")?;
    for (path, row) in rows {
        let trace = row.trace.as_ref().map(json).transpose()?;
        if let Some(sites) = &row.sites_json {
            replace.execute(params![
                path,
                row.facts_digest,
                row.sites_digest,
                row.facts_json,
                trace,
                sites
            ])?;
        } else {
            if row.dirty {
                update_facts.execute(params![path, row.facts_digest, row.facts_json])?;
            }
            if trace.is_some() {
                update_trace.execute(params![path, trace])?;
            }
        }
        let resolved = file_dependencies.get(path);
        if resolved.is_none() && row.trace.is_none() {
            continue;
        }
        let stored = if incremental {
            read_dependencies
                .query_map([path], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<BTreeSet<_>, _>>()?
        } else {
            BTreeSet::new()
        };
        let (stored_traced, stored_resolved): (BTreeSet<_>, BTreeSet<_>) = stored
            .iter()
            .cloned()
            .partition(|key| key.starts_with("t:"));
        let mut wanted = resolved.cloned().unwrap_or(stored_resolved);
        wanted.extend(row.trace.as_ref().map_or(stored_traced, trace_keys));
        for key in stored.difference(&wanted) {
            delete_dependency.execute(params![path, key])?;
        }
        for key in wanted.difference(&stored) {
            insert_dependency.execute(params![path, key])?;
        }
    }
    Ok(())
}

/// One method-call site with its resolution, in writer order: files in path
/// order, sites in extraction order.
struct ResolvedSite<'a> {
    file: &'a str,
    language: &'a str,
    ref_id: String,
    site: &'a SiteHint,
    resolution: Rc<Resolution>,
}

impl ResolvedSite<'_> {
    fn status(&self) -> &'static str {
        if self.resolution.targets.is_empty() {
            "unresolved"
        } else {
            "resolved"
        }
    }

    fn edge_id(&self, target: &Target) -> String {
        format!(
            "dispatch:{}:{}:{}:{}",
            self.file, self.site.ordinal, target.file, target.symbol
        )
    }
}

/// Writes every site's rows, statement by statement. When the signature
/// changed, the site tables and all method-call edges are first emptied.
fn write_row_by_row(
    transaction: &Transaction<'_>,
    sites: &[ResolvedSite<'_>],
    signature_changed: bool,
) -> Result<()> {
    if signature_changed {
        transaction.execute_batch(
            "DELETE FROM view_dispatch_sites; DELETE FROM view_unknown_live;
            DELETE FROM edges WHERE provenance IN ('exact', 'dispatch', 'name_match');",
        )?;
    }
    for site in sites {
        let resolution = &site.resolution;
        transaction.execute(
            "DELETE FROM edges WHERE ref_id=?1 AND provenance NOT IN ('exact', 'dispatch', 'name_match')",
            [&site.ref_id],
        )?;
        transaction.execute("UPDATE refs SET status=?2, target_node=NULL, target_file=NULL, target_symbol=NULL WHERE ref_id=?1", params![site.ref_id, site.status()])?;
        if signature_changed {
            transaction.execute(
                "INSERT INTO view_dispatch_sites VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    site.file,
                    site.site.ordinal,
                    site.language,
                    resolution.unresolved,
                    resolution.dynamic,
                    resolution.external
                ],
            )?;
            for (target_file, target_symbol) in &resolution.protected {
                transaction.execute(
                    "INSERT INTO view_unknown_live VALUES (?1,?2,?3,?4)",
                    params![site.file, site.site.ordinal, target_file, target_symbol],
                )?;
            }
        }
        let caller: Option<String> = site
            .site
            .caller
            .as_ref()
            .map(|symbol| node(transaction, site.file, symbol))
            .transpose()?
            .flatten();
        let Some(caller) = caller else {
            continue;
        };
        for target in &resolution.targets {
            let target_node = node(transaction, &target.file, &target.symbol)?;
            transaction.execute("INSERT OR REPLACE INTO edges (edge_id, ref_id, source_node, target_node, target_file, target_symbol, kind, line, provenance)
                VALUES (?1,?2,?3,?4,?5,?6,'call',?7,?8)", params![site.edge_id(target), site.ref_id, caller, target_node, target.file, target.symbol, site.site.line, target.provenance])?;
        }
    }
    Ok(())
}

/// Row operations performed by [`write_differences`], for profiling.
#[derive(Debug, Default)]
struct DifferenceCounts {
    ordinary_edges_deleted: usize,
    statuses_updated: usize,
    sites_written: usize,
    sites_deleted: usize,
    unknown_live_inserted: usize,
    unknown_live_deleted: usize,
    edges_kept: usize,
    edges_written: usize,
    edges_deleted: usize,
}

type EdgeRow = (String, String, String, i64, String);

/// Brings the method-call rows to exactly what [`write_row_by_row`] leaves in
/// a database whose site tables and method-call edges start empty (a cold
/// build), but writes only rows that differ from the current transaction
/// state.
///
/// The current state already reflects this transaction's ordinary writes:
/// rewritten refs carry ordinary statuses and edges, and the edges of
/// rewritten or relinked refs are gone. Rows of files outside
/// `scope.changed_paths` keep their node IDs, so an existing method-call edge
/// between two such files that matches its site and target needs no lookup.
///
/// With `files`, `sites` are every site of those files and only those files'
/// rows are read and reconciled; other files' rows stay as they are.
fn write_differences(
    transaction: &Transaction<'_>,
    sites: &[ResolvedSite<'_>],
    scope: &EmitScope<'_>,
    files: Option<&BTreeSet<String>>,
) -> Result<DifferenceCounts> {
    let mut counts = DifferenceCounts::default();
    let rewritten = |file: &str| {
        scope
            .changed_paths
            .is_none_or(|paths| paths.contains(file.as_bytes()))
    };

    // Ordinary edges for a site's ref ID exist only if this transaction wrote them.
    {
        let mut delete = transaction.prepare(
            "DELETE FROM edges WHERE ref_id=?1 AND provenance NOT IN ('exact', 'dispatch', 'name_match')",
        )?;
        for site in sites {
            if scope.ordinary_edge_refs.contains(&site.ref_id) {
                counts.ordinary_edges_deleted += delete.execute([&site.ref_id])?;
            }
        }
    }

    // Statuses: every site ref is unresolved or resolved with no target columns.
    {
        let mut current = HashMap::<String, (String, bool)>::new();
        let mut read = transaction.prepare(
            "SELECT ref_id, status, target_node IS NULL AND target_file IS NULL AND target_symbol IS NULL
             FROM refs WHERE caller_file=?1",
        )?;
        let mut file = None;
        let mut update = transaction.prepare("UPDATE refs SET status=?2, target_node=NULL, target_file=NULL, target_symbol=NULL WHERE ref_id=?1")?;
        for site in sites {
            if file != Some(site.file) {
                file = Some(site.file);
                current.clear();
                for row in read.query_map([site.file], |row| {
                    Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
                })? {
                    let (ref_id, state) = row?;
                    current.insert(ref_id, state);
                }
            }
            let Some((status, no_targets)) = current.get(&site.ref_id) else {
                continue;
            };
            if status != site.status() || !no_targets {
                counts.statuses_updated += update.execute(params![site.ref_id, site.status()])?;
            }
        }
    }

    // One `view_dispatch_sites` row per site.
    {
        let mut current = HashMap::<(String, i64), (String, i64, i64, i64)>::new();
        let columns = "SELECT caller_file, ordinal, language, unresolved_receiver_sites,
                    dynamic_member_sites, external FROM view_dispatch_sites";
        let read_row = |row: &rusqlite::Row<'_>| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, i64>(1)?),
                (row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?),
            ))
        };
        let mut read = transaction.prepare(&match files {
            None => columns.to_string(),
            Some(_) => format!("{columns} WHERE caller_file=?1"),
        })?;
        for file in files.map_or_else(|| vec![None], |files| files.iter().map(Some).collect()) {
            let rows = match file {
                None => read.query_map([], read_row)?,
                Some(file) => read.query_map([file], read_row)?,
            };
            for row in rows {
                let (key, value) = row?;
                current.insert(key, value);
            }
        }
        let mut write = transaction
            .prepare("INSERT OR REPLACE INTO view_dispatch_sites VALUES (?1,?2,?3,?4,?5,?6)")?;
        for site in sites {
            let resolution = &site.resolution;
            let wanted = (
                site.language.to_string(),
                resolution.unresolved as i64,
                resolution.dynamic as i64,
                resolution.external as i64,
            );
            if current.remove(&(site.file.to_string(), i64::from(site.site.ordinal)))
                == Some(wanted.clone())
            {
                continue;
            }
            counts.sites_written += write.execute(params![
                site.file,
                site.site.ordinal,
                wanted.0,
                wanted.1,
                wanted.2,
                wanted.3
            ])?;
        }
        let mut delete = transaction
            .prepare("DELETE FROM view_dispatch_sites WHERE caller_file=?1 AND ordinal=?2")?;
        for (file, ordinal) in current.keys() {
            counts.sites_deleted += delete.execute(params![file, ordinal])?;
        }
    }

    // Unknown-receiver liveness: one row per site and protected target.
    {
        let mut wanted = BTreeSet::<(String, i64, String, String)>::new();
        for site in sites {
            for (file, symbol) in &site.resolution.protected {
                wanted.insert((
                    site.file.to_string(),
                    i64::from(site.site.ordinal),
                    file.clone(),
                    symbol.clone(),
                ));
            }
        }
        let mut stale = Vec::new();
        let columns =
            "SELECT caller_file, ordinal, target_file, target_symbol FROM view_unknown_live";
        let read_row = |row: &rusqlite::Row<'_>| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        };
        let mut read = transaction.prepare(&match files {
            None => columns.to_string(),
            Some(_) => format!("{columns} WHERE caller_file=?1"),
        })?;
        for file in files.map_or_else(|| vec![None], |files| files.iter().map(Some).collect()) {
            let rows = match file {
                None => read.query_map([], read_row)?,
                Some(file) => read.query_map([file], read_row)?,
            };
            for row in rows {
                let row = row?;
                if !wanted.remove(&row) {
                    stale.push(row);
                }
            }
        }
        let mut delete = transaction.prepare(
            "DELETE FROM view_unknown_live
             WHERE caller_file=?1 AND ordinal=?2 AND target_file=?3 AND target_symbol=?4",
        )?;
        for row in &stale {
            counts.unknown_live_deleted += delete.execute(params![row.0, row.1, row.2, row.3])?;
        }
        let mut insert =
            transaction.prepare("INSERT INTO view_unknown_live VALUES (?1,?2,?3,?4)")?;
        for row in &wanted {
            counts.unknown_live_inserted += insert.execute(params![row.0, row.1, row.2, row.3])?;
        }
    }

    // Method-call edges.
    {
        let mut current = HashMap::<String, EdgeRow>::new();
        let mut current_targets = HashMap::<String, Option<String>>::new();
        let columns = "SELECT edge_id, ref_id, target_file, target_symbol, kind, line, provenance, target_node
             FROM edges";
        let read_row = |row: &rusqlite::Row<'_>| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get::<_, String>(1)?,
                    format!(
                        "{}\0{}",
                        row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(3)?.unwrap_or_default()
                    ),
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                ),
                row.get::<_, Option<String>>(7)?,
            ))
        };
        // A site's ref ID is `view:{file}:{ordinal}`, so one file's edges are
        // a ref ID range; a longer path sharing the prefix is filtered out.
        let mut read = transaction.prepare(&match files {
            None => format!("{columns} WHERE provenance IN ('exact', 'dispatch', 'name_match')"),
            Some(_) => format!(
                "{columns} INDEXED BY idx_edges_ref_id WHERE ref_id >= ?1 AND ref_id < ?2
                 AND provenance IN ('exact', 'dispatch', 'name_match')"
            ),
        })?;
        for file in files.map_or_else(|| vec![None], |files| files.iter().map(Some).collect()) {
            let rows = match file {
                None => read.query_map([], read_row)?,
                Some(file) => read.query_map(
                    params![format!("view:{file}:"), format!("view:{file};")],
                    read_row,
                )?,
            };
            for row in rows {
                let (edge_id, edge, target_node) = row?;
                if file.is_some_and(|file| {
                    edge.0
                        .strip_prefix("view:")
                        .and_then(|rest| rest.rsplit_once(':'))
                        .is_none_or(|(owner, _)| owner != file)
                }) {
                    continue;
                }
                current_targets.insert(edge_id.clone(), target_node);
                current.insert(edge_id, edge);
            }
        }
        // Several targets of one site can map to one edge ID (same file and
        // symbol, different provenance). Writing them one `INSERT OR REPLACE`
        // at a time keeps the last, so `by_id` keeps the last target per edge
        // ID. Edge IDs of different sites never collide.
        let mut wanted = Vec::<(String, &ResolvedSite<'_>, &Target)>::new();
        for site in sites {
            if site.site.caller.is_none() {
                continue;
            }
            let mut by_id = BTreeMap::new();
            for target in &site.resolution.targets {
                by_id.insert(site.edge_id(target), target);
            }
            let mut ordered = site
                .resolution
                .targets
                .iter()
                .map(|target| site.edge_id(target))
                .collect::<Vec<_>>();
            ordered.dedup();
            for edge_id in ordered {
                if let Some(target) = by_id.remove(&edge_id) {
                    wanted.push((edge_id, site, target));
                }
            }
        }
        let mut callers = HashMap::<(&str, &str), Option<String>>::new();
        let mut write = transaction.prepare("INSERT OR REPLACE INTO edges (edge_id, ref_id, source_node, target_node, target_file, target_symbol, kind, line, provenance)
            VALUES (?1,?2,?3,?4,?5,?6,'call',?7,?8)")?;
        for (edge_id, site, target) in wanted {
            let expected = (
                site.ref_id.clone(),
                format!("{}\0{}", target.file, target.symbol),
                "call".to_string(),
                i64::from(site.site.line),
                target.provenance.to_string(),
            );
            // Rewriting a target's positions does not change its identity.
            // Keep inbound edges only after checking the actual target node,
            // since a kind change at the same scoped name does change identity.
            if !rewritten(site.file)
                && current.get(&edge_id) == Some(&expected)
                && (!rewritten(&target.file)
                    || current_targets.get(&edge_id)
                        == Some(&node(transaction, &target.file, &target.symbol)?))
            {
                current.remove(&edge_id);
                counts.edges_kept += 1;
                continue;
            }
            let symbol = site
                .site
                .caller
                .as_deref()
                .expect("sites without a caller have no edges");
            let caller = match callers.get(&(site.file, symbol)) {
                Some(caller) => caller.clone(),
                None => {
                    let caller = node(transaction, site.file, symbol)?;
                    callers.insert((site.file, symbol), caller.clone());
                    caller
                }
            };
            let Some(caller) = caller else {
                // The row-by-row writer emits no edges for a site whose caller
                // has no node; a current row with this ID is stale.
                continue;
            };
            let target_node = node(transaction, &target.file, &target.symbol)?;
            current.remove(&edge_id);
            counts.edges_written += write.execute(params![
                edge_id,
                site.ref_id,
                caller,
                target_node,
                target.file,
                target.symbol,
                site.site.line,
                target.provenance
            ])?;
        }
        let mut delete = transaction.prepare("DELETE FROM edges WHERE edge_id=?1")?;
        for edge_id in current.keys() {
            counts.edges_deleted += delete.execute([edge_id])?;
        }
    }
    Ok(counts)
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
