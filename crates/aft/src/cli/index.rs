//! One-shot snapshot builder for configured standing index roots.
//!
//! This command deliberately owns no daemon state. It validates the complete
//! configured snapshot, acquires the same per-artifact leases used by standing
//! work, builds every selected entry/kind once, and exits.

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

struct SnapshotUnit {
    entry: StandingRootEntry,
    kind: IndexKind,
    writer_lease: Arc<WriterLease>,
    publish_epoch: ArtifactPublishEpoch,
    publication_epoch: u64,
    admission: UnitAdmission,
    semantic_files: Option<Vec<PathBuf>>,
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
    parse_args(args)?;

    let ResolveResult {
        mut config,
        dropped,
        warnings,
    } = resolve_config(&aft::subc_config::read_local_cortexkit_config_tiers(
        paths.user_config_path.as_deref(),
        &paths.current_dir,
    ));
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
    let plan = prepare_snapshot(config, notices)?;

    writeln!(output, "aft index: snapshot operation").map_err(|error| {
        IndexError::validation(format!("could not write index output: {error}"))
    })?;
    for notice in &plan.notices {
        writeln!(output, "notice: {notice}").map_err(|error| {
            IndexError::validation(format!("could not write index output: {error}"))
        })?;
    }

    let mut aggregate = SnapshotAggregate::default();
    for unit in &plan.units {
        let mut result = match &unit.admission {
            UnitAdmission::Suspended(reason) => UnitResult::gap(reason.clone()),
            UnitAdmission::Admitted { .. } => build_unit(&plan.config, unit),
        };

        if result.usable {
            if let UnitAdmission::Admitted { breaker, key } = &unit.admission {
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

fn prepare_snapshot(config: Config, notices: Vec<String>) -> Result<SnapshotPlan, IndexError> {
    let roots = StandingRoots::default();
    // Reconciliation resolves every configured entry, rejects duplicate artifact
    // identities, and pins all paths in shared aft.db before any build admission.
    let report = roots
        .reconcile(&config)
        .map_err(|error| IndexError::validation(error.to_string()))?;

    let storage_dir = config
        .storage_dir
        .as_deref()
        .expect("snapshot storage directory is set before planning");
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

    let mut semantic_files = BTreeMap::new();
    for entry in &report.active_entries {
        configure_artifact_access(&entry.resolved_target, &entry.artifact_key, false);
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
                writer_lease,
                publish_epoch,
                publication_epoch,
                admission,
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

fn build_unit(config: &Config, unit: &SnapshotUnit) -> UnitResult {
    let result = match unit.kind {
        IndexKind::Search => build_search(config, unit),
        IndexKind::Semantic => build_semantic(config, unit),
        IndexKind::Callgraph => build_callgraph(config, unit),
    };
    match result {
        Ok(result) => result,
        Err(error) => UnitResult::gap(error),
    }
}

fn build_search(config: &Config, unit: &SnapshotUnit) -> Result<UnitResult, String> {
    let storage_dir = config
        .storage_dir
        .as_deref()
        .ok_or_else(|| "snapshot storage directory is unavailable".to_string())?;
    let cache_dir = cache_dir_for(storage_dir, &unit.entry.artifact_key, unit.kind);
    let mut index = SearchIndex::build_with_limit_to_cache_dir(
        &unit.entry.resolved_target,
        config.search_index_max_file_size,
        &cache_dir,
    );
    let head = index.stored_git_head().map(str::to_string);
    let persisted =
        publish_if_current(
            unit,
            || Ok(index.write_to_disk(&cache_dir, head.as_deref())),
        )?;
    if !persisted {
        return Err("could not persist search snapshot".to_string());
    }
    Ok(UnitResult::success(index.file_count()))
}

fn build_semantic(config: &Config, unit: &SnapshotUnit) -> Result<UnitResult, String> {
    let storage_dir = config
        .storage_dir
        .as_deref()
        .ok_or_else(|| "snapshot storage directory is unavailable".to_string())?;
    let files = unit.semantic_files.as_deref().unwrap_or_default();
    let mut model = SemanticEmbeddingModel::from_config(&config.semantic)?;
    let fingerprint = model.fingerprint(&config.semantic)?;
    let embed_text_caps = fingerprint.embed_text_caps;
    let mut embed = |texts: Vec<String>| model.embed(texts);
    let mut index = SemanticIndex::build_with_caps(
        &unit.entry.resolved_target,
        files,
        &mut embed,
        config.semantic.max_batch_size.max(1),
        embed_text_caps,
    )?;
    index.set_fingerprint(fingerprint);
    let persisted = publish_if_current(unit, || {
        Ok(index.write_to_disk(storage_dir, &unit.entry.artifact_key))
    })?;
    if !persisted {
        return Err("could not persist semantic snapshot".to_string());
    }
    Ok(UnitResult::success(index.entry_count()))
}

fn build_callgraph(config: &Config, unit: &SnapshotUnit) -> Result<UnitResult, String> {
    let storage_dir = config
        .storage_dir
        .as_deref()
        .ok_or_else(|| "snapshot storage directory is unavailable".to_string())?;
    let files = callgraph::walk_project_files(&unit.entry.resolved_target).collect::<Vec<_>>();
    let cache_dir = cache_dir_for(storage_dir, &unit.entry.artifact_key, unit.kind);
    let (_store, stats) = publish_if_current(unit, || {
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
    publish: impl FnOnce() -> Result<R, String>,
) -> Result<R, String> {
    unit.publish_epoch
        .run_if_current(unit.publication_epoch, || {
            verify_writer_lease(unit)?;
            let result = publish()?;
            verify_writer_lease(unit)?;
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

fn verify_writer_lease(unit: &SnapshotUnit) -> Result<(), String> {
    unit.writer_lease
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
}
