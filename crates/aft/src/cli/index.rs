//! One-shot snapshot builder for configured standing index roots.
//!
//! This command deliberately owns no daemon state. It validates the complete
//! configured snapshot, acquires the same per-artifact leases used by standing
//! work, builds every selected entry/kind once, and exits. With the data layer
//! on, the kinds it builds take no lease and no breaker admission: the data
//! layer's family store orders its publications with its own writer.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aft::build_breaker::{BreakerAdmission, BreakerKey, BuildDeathBreaker, BuildDomain};
use aft::callgraph;
use aft::callgraph_store::CallGraphStore;
use aft::config::{Config, IndexKind};
use aft::config_resolve::{resolve_config, ConfigWarning, ResolveResult};
use aft::root_cache::{
    configure_artifact_access, ArtifactPublishEpoch, RootCacheDomain, WriterLease,
};
use aft::search_index::{resolve_cache_dir_with_key, SearchIndex};
use aft::semantic_index::{SemanticEmbeddingModel, SemanticIndex};
use aft::standing_roots::{StandingRootEntry, StandingRoots};

/// A finite snapshot has three observable aggregate outcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexExit {
    Success,
    Partial,
    Failure,
}

impl IndexExit {
    pub const fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Partial => 2,
            Self::Failure => 1,
        }
    }
}

#[derive(Debug)]
pub struct IndexError(String);

impl IndexError {
    fn validation(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for IndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for IndexError {}

struct SnapshotPaths {
    current_dir: PathBuf,
    user_config_path: Option<PathBuf>,
    storage_dir: PathBuf,
}

struct SnapshotPlan {
    config: Config,
    roots: StandingRoots,
    units: Vec<SnapshotUnit>,
    notices: Vec<String>,
}

/// Builds the data layer's checkout at a root (the first path), with its
/// family store under a storage folder (the second), in the foreground.
type DataLayerBuild = fn(
    &Path,
    &Path,
    aft::data_layer::checkout::Planes,
) -> Result<aft::data_layer::surfaces::IndexOutcome, String>;

struct SnapshotUnit {
    entry: StandingRootEntry,
    kind: IndexKind,
    writer: UnitWriter,
    semantic_files: Option<Vec<PathBuf>>,
}

/// What builds one unit, and what fences its publication.
enum UnitWriter {
    /// Today's stores: a writer lease shared with standing work, a publish
    /// epoch, and the build breaker's admission.
    Leased(LeasedWriter),
    /// The data layer, whose family store orders publications with its own
    /// writer.
    DataLayer(DataLayerBuild),
}

struct LeasedWriter {
    writer_lease: Arc<WriterLease>,
    publish_epoch: ArtifactPublishEpoch,
    publication_epoch: u64,
    admission: UnitAdmission,
}

enum UnitAdmission {
    Admitted {
        breaker: BuildDeathBreaker,
        key: BreakerKey,
    },
    Suspended(String),
}

struct UnitResult {
    usable: bool,
    gap: Option<String>,
    result_count: usize,
}

impl UnitResult {
    fn success(result_count: usize) -> Self {
        Self {
            usable: true,
            gap: None,
            result_count,
        }
    }

    fn usable_gap(result_count: usize, gap: impl Into<String>) -> Self {
        Self {
            usable: true,
            gap: Some(gap.into()),
            result_count,
        }
    }

    fn gap(gap: impl Into<String>) -> Self {
        Self {
            usable: false,
            gap: Some(gap.into()),
            result_count: 0,
        }
    }
}

/// Execute the bare `aft index` command. It has no root or index filters: the
/// configured snapshot is the complete work set.
pub fn run(args: Vec<OsString>) -> Result<IndexExit, IndexError> {
    let current_dir = std::env::current_dir().map_err(|error| {
        IndexError::validation(format!("could not determine current directory: {error}"))
    })?;
    let paths = SnapshotPaths {
        current_dir,
        user_config_path: aft::subc_config::cortexkit_user_config_path(),
        storage_dir: aft::bash_background::storage_dir(None),
    };
    run_with_paths(args, paths, &mut io::stdout().lock())
}

fn run_with_paths(
    args: Vec<OsString>,
    paths: SnapshotPaths,
    output: &mut impl Write,
) -> Result<IndexExit, IndexError> {
    let data_layer = aft::data_layer::surfaces::enabled()
        .then_some(aft::data_layer::surfaces::index_root as DataLayerBuild);
    run_snapshot(args, paths, data_layer, output)
}

/// Runs the snapshot, building the kinds the data layer builds with
/// `data_layer` when it is on, and every kind with today's stores when it
/// is `None`.
fn run_snapshot(
    args: Vec<OsString>,
    paths: SnapshotPaths,
    data_layer: Option<DataLayerBuild>,
    output: &mut impl Write,
) -> Result<IndexExit, IndexError> {
    parse_args(args)?;

    let ResolveResult {
        mut config,
        dropped,
        warnings,
        errors,
    } = resolve_config(&aft::subc_config::read_local_cortexkit_config_tiers(
        paths.user_config_path.as_deref(),
        &paths.current_dir,
    ));
    if !errors.is_empty() {
        return Err(IndexError::validation(format!(
            "configuration rejected: {}",
            errors.join(", ")
        )));
    }
    reject_index_refusals(&dropped, &warnings)?;

    if config.index.roots.is_empty() {
        // This exact one-line response distinguishes a deliberate snapshot no-op
        // from a daemon or scheduler that will eventually discover work.
        writeln!(output, "no standing entries configured").map_err(|error| {
            IndexError::validation(format!("could not write index output: {error}"))
        })?;
        return Ok(IndexExit::Success);
    }

    config.storage_dir = Some(paths.storage_dir);
    let notices = index_notices(&warnings);
    if let Some(warning) = aft::root_cache::prepare_cache_root(&mut config) {
        return Err(IndexError::validation(format!(
            "cache_dir_unavailable: {}",
            warning.message
        )));
    }
    let plan = prepare_snapshot(config, notices, data_layer)?;

    writeln!(output, "aft index: snapshot operation").map_err(|error| {
        IndexError::validation(format!("could not write index output: {error}"))
    })?;
    for notice in &plan.notices {
        writeln!(output, "notice: {notice}").map_err(|error| {
            IndexError::validation(format!("could not write index output: {error}"))
        })?;
    }

    let mut aggregate = SnapshotAggregate::default();
    let mut data_layer_builds = DataLayerBuilds::new();
    for unit in &plan.units {
        let mut result = match &unit.writer {
            UnitWriter::DataLayer(build) => {
                build_data_layer_unit(&plan.config, unit, *build, &mut data_layer_builds)
            }
            UnitWriter::Leased(writer) => match &writer.admission {
                UnitAdmission::Suspended(reason) => UnitResult::gap(reason.clone()),
                UnitAdmission::Admitted { .. } => build_unit(&plan.config, unit, writer),
            },
        };

        if result.usable {
            if let UnitWriter::Leased(LeasedWriter {
                admission: UnitAdmission::Admitted { breaker, key },
                ..
            }) = &unit.writer
            {
                if let Err(error) = breaker.record_ready_publication(key) {
                    result.gap = Some(format!("could not record breaker publication: {error}"));
                }
            }
        }

        // A full snapshot build reads every participating source file while it is
        // constructing the artifact, so this is the strict-current-state proof
        // that consumes the durable freshness flag. Partial output never clears it.
        if result.usable && result.gap.is_none() {
            if let Err(error) = plan
                .roots
                .record_strict_verification(&unit.entry.literal_path, unit.kind)
            {
                result.gap = Some(format!("could not record strict verification: {error}"));
            }
        }

        write_disclosure(output, unit, &result)?;
        aggregate.record(&result);
    }

    Ok(aggregate.exit())
}

fn parse_args(args: Vec<OsString>) -> Result<(), IndexError> {
    let Some(first) = args.into_iter().next() else {
        return Ok(());
    };
    let first = first
        .into_string()
        .map_err(|_| IndexError::validation("index arguments must be valid UTF-8"))?;
    Err(IndexError::validation(format!(
        "index is a bare snapshot operation and accepts no root or index filter flags (unexpected argument: {first})"
    )))
}

fn reject_index_refusals(
    dropped: &[aft::config_resolve::DroppedKey],
    warnings: &[ConfigWarning],
) -> Result<(), IndexError> {
    if let Some(drop) = dropped.iter().find(|drop| drop.key == "index.roots") {
        return Err(IndexError::validation(format!(
            "index.roots trust refusal from {} tier: {}",
            drop.tier, drop.reason
        )));
    }
    if let Some(warning) = warnings
        .iter()
        .find(|warning| warning.code == "invalid_index_roots")
    {
        return Err(IndexError::validation(format!(
            "index.roots validation refusal: {}",
            warning.message
        )));
    }
    Ok(())
}

fn index_notices(warnings: &[ConfigWarning]) -> Vec<String> {
    warnings
        .iter()
        .filter(|warning| warning.key == "index.roots")
        .map(|warning| warning.message.clone())
        .collect()
}

fn prepare_snapshot(
    config: Config,
    notices: Vec<String>,
    data_layer: Option<DataLayerBuild>,
) -> Result<SnapshotPlan, IndexError> {
    let roots = StandingRoots::default();
    // Reconciliation resolves every configured entry, rejects duplicate artifact
    // identities, and pins all paths in shared aft.db before any build admission.
    let report = roots
        .reconcile(&config)
        .map_err(|error| IndexError::validation(error.to_string()))?;

    let cache_root = aft::root_cache::cache_root(&config);
    let storage_dir = cache_root.as_path();
    let has_semantic = report
        .active_entries
        .iter()
        .any(|entry| entry.indexes.contains(&IndexKind::Semantic));
    if has_semantic {
        // Construction checks backend configuration and credential availability
        // without admitting an artifact build. A bad semantic configuration must
        // therefore reject the complete snapshot before another unit can build.
        SemanticEmbeddingModel::from_config(&config.semantic).map_err(|error| {
            IndexError::validation(format!("semantic validation refusal: {error}"))
        })?;
    }

    // The data layer builds a kind when it is on and builds that kind;
    // today's stores build the rest.
    let data_layer_for =
        |kind: IndexKind| data_layer.filter(|_| aft::data_layer::standing::builds(kind));
    let mut semantic_files = BTreeMap::new();
    for entry in &report.active_entries {
        // The write capability only today's writer leases need.
        if entry
            .indexes
            .iter()
            .any(|kind| data_layer_for(*kind).is_none())
        {
            configure_artifact_access(&entry.resolved_target, &entry.artifact_key, false);
        }
        if entry.indexes.contains(&IndexKind::Semantic) {
            semantic_files.insert(
                entry.literal_path.clone(),
                collect_semantic_files(&entry.resolved_target, config.semantic.max_files)?,
            );
        }
    }

    let mut units = Vec::new();
    for entry in report.active_entries {
        for kind in &entry.indexes {
            if let Some(build) = data_layer_for(*kind) {
                units.push(SnapshotUnit {
                    entry: entry.clone(),
                    kind: *kind,
                    writer: UnitWriter::DataLayer(build),
                    semantic_files: None,
                });
                continue;
            }
            let cache_dir = cache_dir_for(storage_dir, &entry.artifact_key, *kind);
            let domain = if *kind == IndexKind::Callgraph {
                RootCacheDomain::Callgraph
            } else {
                RootCacheDomain::Index
            };
            // Holding the lease across this finite build applies the same local
            // filesystem refusal and cross-process writer fencing as standing work.
            let writer_lease = WriterLease::acquire_shared(
                domain,
                &cache_dir,
                &entry.artifact_key,
                &entry.resolved_target,
            )
            .map_err(|error| {
                IndexError::validation(format!(
                    "index writer-lease refusal for {:?} ({}): {error}",
                    entry.literal_path,
                    kind.as_str()
                ))
            })?
            .ok_or_else(|| {
                IndexError::validation(format!(
                    "index writer capability refusal for {:?} ({})",
                    entry.literal_path,
                    kind.as_str()
                ))
            })?;
            let admission = admit_breaker(&cache_dir, &entry, *kind)?;
            let publish_epoch = ArtifactPublishEpoch::default();
            let publication_epoch = publish_epoch.next();
            units.push(SnapshotUnit {
                semantic_files: (*kind == IndexKind::Semantic).then(|| {
                    semantic_files
                        .remove(&entry.literal_path)
                        .unwrap_or_default()
                }),
                entry: entry.clone(),
                kind: *kind,
                writer: UnitWriter::Leased(LeasedWriter {
                    writer_lease,
                    publish_epoch,
                    publication_epoch,
                    admission,
                }),
            });
        }
    }

    Ok(SnapshotPlan {
        config,
        roots,
        units,
        notices,
    })
}

fn collect_semantic_files(root: &Path, max_files: usize) -> Result<Vec<PathBuf>, IndexError> {
    let mut files = callgraph::walk_project_files(root)
        .filter(|path| aft::semantic_index::is_semantic_indexed_extension(path))
        .collect::<Vec<_>>();
    files.sort();
    if files.len() > max_files {
        return Err(IndexError::validation(format!(
            "semantic validation refusal for {}: {} files exceeds configured maximum {}",
            root.display(),
            files.len(),
            max_files
        )));
    }
    Ok(files)
}

fn admit_breaker(
    cache_dir: &Path,
    entry: &StandingRootEntry,
    kind: IndexKind,
) -> Result<UnitAdmission, IndexError> {
    let files = callgraph::walk_project_files(&entry.resolved_target).collect::<Vec<_>>();
    let fingerprint = snapshot_fingerprint(&entry.resolved_target, kind, &files);
    let key = BreakerKey::new(
        entry.artifact_key.clone(),
        breaker_domain(kind),
        fingerprint,
    );
    let breaker =
        BuildDeathBreaker::open(cache_dir.join("build-breaker.sqlite")).map_err(|error| {
            IndexError::validation(format!("could not open index breaker: {error}"))
        })?;
    match breaker.admit(&key, 0).map_err(|error| {
        IndexError::validation(format!("could not check index breaker: {error}"))
    })? {
        BreakerAdmission::Admitted(_) => Ok(UnitAdmission::Admitted { breaker, key }),
        BreakerAdmission::Suspended(suspension) => Ok(UnitAdmission::Suspended(format!(
            "build suspended for {}: {}",
            suspension.domain.as_str(),
            suspension.reason
        ))),
    }
}

fn breaker_domain(kind: IndexKind) -> BuildDomain {
    match kind {
        IndexKind::Search => BuildDomain::SearchCold,
        IndexKind::Semantic => BuildDomain::SemanticSeed,
        IndexKind::Callgraph => BuildDomain::CallgraphCold,
    }
}

fn snapshot_fingerprint(root: &Path, kind: IndexKind, files: &[PathBuf]) -> String {
    let mut files = files.to_vec();
    files.sort();
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update(&[0]);
    for file in files {
        let relative = file.strip_prefix(root).unwrap_or(&file);
        hasher.update(relative.to_string_lossy().as_bytes());
        if let Ok(metadata) = std::fs::metadata(&file) {
            hasher.update(&metadata.len().to_le_bytes());
            if let Ok(modified) = metadata.modified() {
                if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
                    hasher.update(&duration.as_nanos().to_le_bytes());
                }
            }
        }
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn cache_dir_for(storage_dir: &Path, artifact_key: &str, kind: IndexKind) -> PathBuf {
    match kind {
        IndexKind::Search => resolve_cache_dir_with_key(artifact_key, Some(storage_dir)),
        IndexKind::Semantic => storage_dir.join("semantic").join(artifact_key),
        IndexKind::Callgraph => storage_dir.join("callgraph").join(artifact_key),
    }
}

/// Each entry's data-layer build, by literal path: one build publishes both
/// the text and the call-graph plane, so an entry that selects both kinds
/// builds once.
type DataLayerBuilds = BTreeMap<String, Result<aft::data_layer::surfaces::IndexOutcome, String>>;

/// The search or call-graph kind of `unit`, built by the data layer with
/// `build`: the checkout's text and call-graph planes, published to the
/// repository's family store in the foreground.
fn build_data_layer_unit(
    config: &Config,
    unit: &SnapshotUnit,
    build: DataLayerBuild,
    builds: &mut DataLayerBuilds,
) -> UnitResult {
    let outcome = builds
        .entry(unit.entry.literal_path.clone())
        .or_insert_with(|| {
            let root = std::fs::canonicalize(&unit.entry.resolved_target)
                .unwrap_or_else(|_| unit.entry.resolved_target.clone());
            let storage = aft::bash_background::storage_dir(config.storage_dir.as_deref());
            build(
                &root,
                &storage,
                aft::data_layer::standing::planes(&unit.entry.indexes),
            )
        });
    match (outcome, unit.kind) {
        (Err(error), _) => UnitResult::gap(error.clone()),
        (Ok(outcome), IndexKind::Callgraph) if outcome.unparsed_files > 0 => {
            UnitResult::usable_gap(
                outcome.parsed_files,
                format!(
                    "callgraph could not parse {} file(s)",
                    outcome.unparsed_files
                ),
            )
        }
        (Ok(outcome), IndexKind::Callgraph) => UnitResult::success(outcome.parsed_files),
        (Ok(outcome), _) => UnitResult::success(outcome.files),
    }
}

fn build_unit(config: &Config, unit: &SnapshotUnit, writer: &LeasedWriter) -> UnitResult {
    let result = match unit.kind {
        IndexKind::Search => build_search(config, unit, writer),
        IndexKind::Semantic => build_semantic(config, unit, writer),
        IndexKind::Callgraph => build_callgraph(config, unit, writer),
    };
    match result {
        Ok(result) => result,
        Err(error) => UnitResult::gap(error),
    }
}

fn build_search(
    config: &Config,
    unit: &SnapshotUnit,
    writer: &LeasedWriter,
) -> Result<UnitResult, String> {
    let cache_root = aft::root_cache::cache_root(config);
    let storage_dir = cache_root.as_path();
    let cache_dir = cache_dir_for(storage_dir, &unit.entry.artifact_key, unit.kind);
    let mut index = SearchIndex::build_with_limit_to_cache_dir(
        &unit.entry.resolved_target,
        config.search_index_max_file_size,
        &cache_dir,
    );
    let head = index.stored_git_head().map(str::to_string);
    let persisted = publish_if_current(unit, writer, || {
        Ok(index.write_to_disk(&cache_dir, head.as_deref()))
    })?;
    if !persisted {
        return Err("could not persist search snapshot".to_string());
    }
    Ok(UnitResult::success(index.file_count()))
}

fn build_semantic(
    config: &Config,
    unit: &SnapshotUnit,
    writer: &LeasedWriter,
) -> Result<UnitResult, String> {
    let cache_root = aft::root_cache::cache_root(config);
    let storage_dir = cache_root.as_path();
    let files = unit.semantic_files.as_deref().unwrap_or_default();
    let mut model = SemanticEmbeddingModel::from_config(&config.semantic)?;
    let fingerprint = model.fingerprint(&config.semantic)?;
    let embed_text_caps = fingerprint.embed_text_caps;
    let mut embed = |texts: Vec<String>| model.embed(texts);
    let mut index = SemanticIndex::build_with_caps_for_key(
        &unit.entry.resolved_target,
        files,
        &mut embed,
        config.semantic.max_batch_size.max(1),
        embed_text_caps,
        &unit.entry.artifact_key,
    )?;
    index.set_fingerprint(fingerprint);
    let persisted = publish_if_current(unit, writer, || {
        Ok(index.write_to_disk(storage_dir, &unit.entry.artifact_key))
    })?;
    if !persisted {
        return Err("could not persist semantic snapshot".to_string());
    }
    Ok(UnitResult::success(index.entry_count()))
}

fn build_callgraph(
    config: &Config,
    unit: &SnapshotUnit,
    writer: &LeasedWriter,
) -> Result<UnitResult, String> {
    let cache_root = aft::root_cache::cache_root(config);
    let storage_dir = cache_root.as_path();
    let files = callgraph::walk_project_files(&unit.entry.resolved_target).collect::<Vec<_>>();
    let cache_dir = cache_dir_for(storage_dir, &unit.entry.artifact_key, unit.kind);
    let (_store, stats) = publish_if_current(unit, writer, || {
        CallGraphStore::cold_build_with_lease_chunked(
            cache_dir,
            unit.entry.resolved_target.clone(),
            &files,
            config.callgraph_chunk_size,
        )
        .map_err(|error| error.to_string())
    })?;
    if stats.failed_files.is_empty() {
        Ok(UnitResult::success(stats.files))
    } else {
        Ok(UnitResult::usable_gap(
            stats.files,
            format!(
                "callgraph skipped {} unreadable file(s)",
                stats.failed_files.len()
            ),
        ))
    }
}

/// Hold the publish mutex across the lease/freshness check and final artifact
/// write. Snapshot workers do not schedule a successor, but using the same lock
/// and publication order as standing work prevents races with another in-process
/// caller publishing the same artifact.
fn publish_if_current<R>(
    unit: &SnapshotUnit,
    writer: &LeasedWriter,
    publish: impl FnOnce() -> Result<R, String>,
) -> Result<R, String> {
    writer
        .publish_epoch
        .run_if_current(writer.publication_epoch, || {
            verify_writer_lease(unit, writer)?;
            let result = publish()?;
            verify_writer_lease(unit, writer)?;
            Ok(result)
        })
        .ok_or_else(|| {
            format!(
                "publication epoch changed for {:?} ({})",
                unit.entry.literal_path,
                unit.kind.as_str()
            )
        })?
}

fn verify_writer_lease(unit: &SnapshotUnit, writer: &LeasedWriter) -> Result<(), String> {
    writer
        .writer_lease
        .verify()
        .map_err(|error| {
            format!(
                "writer lease verification failed for {:?} ({}): {error}",
                unit.entry.literal_path,
                unit.kind.as_str()
            )
        })?
        .then_some(())
        .ok_or_else(|| {
            format!(
                "writer lease epoch changed for {:?} ({})",
                unit.entry.literal_path,
                unit.kind.as_str()
            )
        })
}

fn write_disclosure(
    output: &mut impl Write,
    unit: &SnapshotUnit,
    result: &UnitResult,
) -> Result<(), IndexError> {
    match &result.gap {
        Some(gap) => writeln!(
            output,
            "snapshot entry={:?} kind={} status=gap usable={} result_count={} reason={:?}",
            unit.entry.literal_path,
            unit.kind.as_str(),
            result.usable,
            result.result_count,
            gap
        ),
        None => writeln!(
            output,
            "snapshot entry={:?} kind={} status=success result_count={}",
            unit.entry.literal_path,
            unit.kind.as_str(),
            result.result_count
        ),
    }
    .map_err(|error| IndexError::validation(format!("could not write index output: {error}")))
}

#[derive(Default)]
struct SnapshotAggregate {
    usable: bool,
    gaps: bool,
}

impl SnapshotAggregate {
    fn record(&mut self, result: &UnitResult) {
        self.usable |= result.usable;
        self.gaps |= result.gap.is_some();
    }

    fn exit(self) -> IndexExit {
        match (self.usable, self.gaps) {
            (_, false) => IndexExit::Success,
            (true, true) => IndexExit::Partial,
            (false, true) => IndexExit::Failure,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aft::data_layer::surfaces::IndexOutcome;
    use aft::db::standing_roots::needs_strict_verify;
    use aft::scoped_key::resolve_standing_root;
    use tempfile::tempdir;

    fn paths(root: &Path, user_config: &Path, storage: &Path) -> SnapshotPaths {
        SnapshotPaths {
            current_dir: root.to_path_buf(),
            user_config_path: Some(user_config.to_path_buf()),
            storage_dir: storage.to_path_buf(),
        }
    }

    fn write_user_config(path: &Path, source: &str) {
        std::fs::write(path, source).unwrap();
    }

    #[test]
    fn bare_empty_snapshot_is_an_explicit_successful_noop() {
        let fixture = tempdir().unwrap();
        let user = fixture.path().join("user-aft.jsonc");
        write_user_config(&user, "{}");
        let mut output = Vec::new();

        let exit = run_with_paths(
            Vec::new(),
            paths(fixture.path(), &user, &fixture.path().join("storage")),
            &mut output,
        )
        .unwrap();

        assert_eq!(exit, IndexExit::Success);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "no standing entries configured\n"
        );
    }

    #[test]
    fn root_and_index_filters_are_refused_before_snapshot_planning() {
        let error = parse_args(vec![OsString::from("--root")]).unwrap_err();
        assert!(error.to_string().contains("no root or index filter flags"));
        let error = parse_args(vec![OsString::from("--only=search")]).unwrap_err();
        assert!(error.to_string().contains("no root or index filter flags"));
    }

    #[test]
    fn validation_and_project_tier_trust_refusals_admit_no_builds() {
        let fixture = tempdir().unwrap();
        let storage = fixture.path().join("storage");
        let user = fixture.path().join("user-aft.jsonc");
        write_user_config(
            &user,
            r#"{ "index": { "roots": [{ "path": "relative", "indexes": ["search"] }] } }"#,
        );
        let error = run_with_paths(
            Vec::new(),
            paths(fixture.path(), &user, &storage),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("validation refusal"));
        assert!(
            !storage.join("index").exists(),
            "a validation refusal must happen before any artifact admission"
        );

        let source = fixture.path().join("source");
        std::fs::create_dir(&source).unwrap();
        write_user_config(
            &user,
            &format!(
                r#"{{ "index": {{ "roots": [{{ "path": {:?}, "indexes": ["search"] }}] }} }}"#,
                source
            ),
        );
        std::fs::create_dir(fixture.path().join(".cortexkit")).unwrap();
        std::fs::write(
            fixture.path().join(".cortexkit/aft.jsonc"),
            format!(
                r#"{{ "index": {{ "roots": [{{ "path": {:?}, "indexes": ["search"] }}] }} }}"#,
                fixture.path()
            ),
        )
        .unwrap();
        let error = run_with_paths(
            Vec::new(),
            paths(fixture.path(), &user, &storage),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("trust refusal"));
        assert!(
            !storage.join("index").exists(),
            "a trust refusal must happen before any artifact admission"
        );
    }

    #[test]
    fn search_snapshot_discloses_zero_results_and_clears_shared_strict_state() {
        let fixture = tempdir().unwrap();
        let source = fixture.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let storage = fixture.path().join("storage");
        let user = fixture.path().join("user-aft.jsonc");
        write_user_config(
            &user,
            &format!(
                r#"{{ "index": {{ "roots": [{{ "path": {:?}, "indexes": ["search"] }}] }} }}"#,
                source
            ),
        );
        let mut output = Vec::new();

        let exit = run_with_paths(
            Vec::new(),
            paths(fixture.path(), &user, &storage),
            &mut output,
        )
        .unwrap();

        assert_eq!(exit, IndexExit::Success);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("aft index: snapshot operation"));
        assert!(output.contains("kind=search status=success result_count=0"));

        let literal_path = source.to_string_lossy().into_owned();
        let identity = resolve_standing_root(&literal_path).unwrap();
        assert!(storage
            .join("index")
            .join(identity.artifact_key)
            .join("cache.bin")
            .exists());
        let conn = aft::db::open(&storage.join("aft.db")).unwrap();
        assert_eq!(
            needs_strict_verify(&conn, &literal_path, IndexKind::Search).unwrap(),
            Some(false),
            "the CLI and standing actor use the same durable freshness row"
        );
    }

    #[test]
    fn aggregate_exit_states_preserve_usable_partial_output() {
        let mut complete = SnapshotAggregate::default();
        complete.record(&UnitResult::success(0));
        assert_eq!(complete.exit(), IndexExit::Success);

        let mut partial = SnapshotAggregate::default();
        partial.record(&UnitResult::success(1));
        partial.record(&UnitResult::gap("missing"));
        assert_eq!(partial.exit(), IndexExit::Partial);

        let mut failed = SnapshotAggregate::default();
        failed.record(&UnitResult::gap("missing"));
        assert_eq!(failed.exit(), IndexExit::Failure);
    }

    /// A small Rust crate configured as one standing entry with `indexes`;
    /// returns its literal path in the configuration.
    fn standing_crate(fixture: &Path, user: &Path, indexes: &str) -> String {
        let source = fixture.join("source");
        for (path, text) in [
            ("Cargo.toml", "[package]\nname = \"demo\"\n"),
            (
                "src/lib.rs",
                "mod util;\npub fn caller() { util::callee(); }\n",
            ),
            ("src/util.rs", "pub fn callee() {}\n"),
        ] {
            let path = source.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        write_user_config(
            user,
            &format!(
                r#"{{ "index": {{ "roots": [{{ "path": {source:?}, "indexes": {indexes} }}] }} }}"#
            ),
        );
        source.to_string_lossy().into_owned()
    }

    fn strict(storage: &Path, literal_path: &str, kind: IndexKind) -> Option<bool> {
        let conn = aft::db::open(&storage.join("aft.db")).unwrap();
        needs_strict_verify(&conn, literal_path, kind).unwrap()
    }

    #[test]
    fn data_layer_snapshot_takes_no_lease_or_breaker_and_clears_strict_state() {
        let fixture = tempdir().unwrap();
        let storage = fixture.path().join("storage");
        let user = fixture.path().join("user-aft.jsonc");
        let literal_path = standing_crate(fixture.path(), &user, r#"["search", "callgraph"]"#);
        let mut output = Vec::new();

        let exit = run_snapshot(
            Vec::new(),
            paths(fixture.path(), &user, &storage),
            Some(aft::data_layer::surfaces::index_root),
            &mut output,
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert_eq!(exit, IndexExit::Success, "{output}");
        assert!(output.starts_with("aft index: snapshot operation\n"));
        assert!(
            output.contains("kind=search status=success result_count=3"),
            "{output}"
        );
        assert!(
            output.contains("kind=callgraph status=success result_count=2"),
            "{output}"
        );
        assert!(storage.join("data").is_dir());
        // Today's cache folders hold the search cache, the call-graph store,
        // their writer leases and the build breaker: none is created.
        let identity = resolve_standing_root(&literal_path).unwrap();
        for kind in [IndexKind::Search, IndexKind::Callgraph] {
            let dir = cache_dir_for(&storage, &identity.artifact_key, kind);
            assert!(!dir.exists(), "{}", dir.display());
            assert_eq!(strict(&storage, &literal_path, kind), Some(false));
        }
    }

    #[test]
    fn data_layer_snapshot_with_an_unparsed_file_is_partial_and_keeps_its_strict_state() {
        fn one_unparsed(
            _root: &Path,
            _storage: &Path,
            _planes: aft::data_layer::checkout::Planes,
        ) -> Result<IndexOutcome, String> {
            Ok(IndexOutcome {
                files: 3,
                parsed_files: 1,
                unparsed_files: 1,
                published: true,
            })
        }
        let fixture = tempdir().unwrap();
        let storage = fixture.path().join("storage");
        let user = fixture.path().join("user-aft.jsonc");
        let literal_path = standing_crate(fixture.path(), &user, r#"["search", "callgraph"]"#);
        let mut output = Vec::new();

        let exit = run_snapshot(
            Vec::new(),
            paths(fixture.path(), &user, &storage),
            Some(one_unparsed),
            &mut output,
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert_eq!(exit, IndexExit::Partial, "{output}");
        assert_eq!(exit.code(), 2);
        assert!(
            output.contains("kind=search status=success result_count=3"),
            "{output}"
        );
        assert!(
            output.contains(
                "kind=callgraph status=gap usable=true result_count=1 reason=\"callgraph could not parse 1 file(s)\""
            ),
            "{output}"
        );
        assert_eq!(
            strict(&storage, &literal_path, IndexKind::Search),
            Some(false)
        );
        assert_eq!(
            strict(&storage, &literal_path, IndexKind::Callgraph),
            Some(true),
            "partial output never clears the flag"
        );
    }

    #[test]
    fn data_layer_snapshot_that_cannot_open_its_store_fails_and_keeps_strict_state() {
        let fixture = tempdir().unwrap();
        let storage = fixture.path().join("storage");
        let user = fixture.path().join("user-aft.jsonc");
        let literal_path = standing_crate(fixture.path(), &user, r#"["search", "callgraph"]"#);
        // A file where the data layer's stores go: no family store opens.
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("data"), "").unwrap();
        let mut output = Vec::new();

        let exit = run_snapshot(
            Vec::new(),
            paths(fixture.path(), &user, &storage),
            Some(aft::data_layer::surfaces::index_root),
            &mut output,
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert_eq!(exit, IndexExit::Failure, "{output}");
        assert_eq!(exit.code(), 1);
        let identity = resolve_standing_root(&literal_path).unwrap();
        for kind in [IndexKind::Search, IndexKind::Callgraph] {
            assert!(
                output.contains(&format!(
                    "kind={} status=gap usable=false result_count=0 reason=",
                    kind.as_str()
                )),
                "{output}"
            );
            assert_eq!(strict(&storage, &literal_path, kind), Some(true));
            let dir = cache_dir_for(&storage, &identity.artifact_key, kind);
            assert!(
                !dir.exists(),
                "fell back to today's stores: {}",
                dir.display()
            );
        }
    }

    #[test]
    fn with_the_data_layer_off_a_snapshot_builds_todays_stores_under_leases() {
        let fixture = tempdir().unwrap();
        let storage = fixture.path().join("storage");
        let user = fixture.path().join("user-aft.jsonc");
        let literal_path = standing_crate(fixture.path(), &user, r#"["search", "callgraph"]"#);
        let mut output = Vec::new();

        // The process switch, which is off here.
        let exit = run_with_paths(
            Vec::new(),
            paths(fixture.path(), &user, &storage),
            &mut output,
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert_eq!(exit, IndexExit::Success, "{output}");
        assert!(output.contains("kind=search status=success"), "{output}");
        assert!(output.contains("kind=callgraph status=success"), "{output}");
        assert!(!storage.join("data").exists());
        let identity = resolve_standing_root(&literal_path).unwrap();
        let search = cache_dir_for(&storage, &identity.artifact_key, IndexKind::Search);
        assert!(search.join("cache.bin").is_file());
        for kind in [IndexKind::Search, IndexKind::Callgraph] {
            let dir = cache_dir_for(&storage, &identity.artifact_key, kind);
            assert!(
                dir.join("build-breaker.sqlite").is_file(),
                "{}",
                dir.display()
            );
            assert_eq!(strict(&storage, &literal_path, kind), Some(false));
        }
    }
}
