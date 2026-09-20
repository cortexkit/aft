use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::{Connection, OptionalExtension};

use crate::alias::{head_tree_entries, AliasStore, GitMode};
use crate::blob_store::{
    BlobPlane, BlobStore, CallgraphKey, FullKey, PutOutcome, CALLGRAPH_PRODUCER_VERSION,
};
use crate::callgraph_store::join::CallgraphBlob;
use crate::parser::detect_language;
use crate::path_status::PathStatusStore;
use crate::pins::AssemblyPin;

use super::{
    ArtifactPlane, ByteString, ClosureRequirements, Manifest, ManifestEntry, PublicationArtifacts,
    PublicationClosure, PublicationRequest, PublishOutcome, RegularPlanes, RelPath, Result,
    ViewError, ViewStore,
};

#[derive(Clone, Debug)]
pub struct AssemblyRequest {
    pub storage: PathBuf,
    pub project_root: PathBuf,
    pub family: String,
    pub scope: String,
    pub desired_head: String,
    pub changed_paths: BTreeSet<Vec<u8>>,
    pub semantic_keys: BTreeMap<Vec<u8>, String>,
    pub require_semantic: bool,
    pub allow_blob_put: bool,
}

#[derive(Clone, Debug)]
pub struct AssemblyReport {
    pub generation: Option<String>,
    pub manifest: Option<Manifest>,
    pub blob_puts: usize,
    pub pending_paths: BTreeSet<Vec<u8>>,
    pub published: bool,
}

struct Candidate {
    path: RelPath,
    entry: ManifestEntry,
    key: Option<FullKey>,
    payload: Option<Vec<u8>>,
    tracked: Option<crate::alias::TrackedPath>,
    source: Option<Vec<u8>>,
}

pub fn head_tree_fingerprint(entries: &[crate::alias::TrackedPath]) -> String {
    let mut hasher = blake3::Hasher::new();
    for entry in entries {
        hasher.update(&(entry.rel_path.len() as u64).to_le_bytes());
        hasher.update(&entry.rel_path);
        hasher.update(entry.mode.as_bytes());
        hasher.update(entry.git_oid.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

pub fn publish_checkout(request: &AssemblyRequest) -> Result<AssemblyReport> {
    let mut prepared = prepare_checkout(request, &mut |_| Ok(()))?;
    prepared.commit()
}

/// Owns the unpublished files and assembly pin until CAS succeeds or the build
/// is dropped. Cancellation and errors follow the same cleanup path.
pub struct PreparedAssembly {
    report: AssemblyReport,
    publication: Option<super::PreparedPublication>,
    files: Option<(ViewStore, String)>,
    derived_checkpoint: Option<(PathBuf, crate::db::file_identity::IdentityConnection)>,
    pin: Option<AssemblyPin>,
    _base_pin: Option<crate::pins::QueryPin>,
    profile: PublicationProfile,
}

impl PreparedAssembly {
    pub fn report(&self) -> &AssemblyReport {
        &self.report
    }

    pub fn commit(&mut self) -> Result<AssemblyReport> {
        if let Some(publication) = self.publication.take() {
            let retires_legacy_plane = publication.base_generation.is_none();
            let pointer_started = Instant::now();
            let outcome = publication.commit();
            self.profile.pointer_ms = pointer_started.elapsed().as_millis();
            match outcome? {
                PublishOutcome::Published => {
                    self.report.published = true;
                    self.files = None;
                    if retires_legacy_plane {
                        log::info!(
                            "views: root={} legacy plane retired at generation={}",
                            self.profile.root.display(),
                            self.report
                                .generation
                                .as_deref()
                                .expect("prepared publication has a generation")
                        );
                    }
                    if let Some((path, connection)) = self.derived_checkpoint.take() {
                        self.profile.finish();
                        super::generation::schedule_derived_checkpoint(
                            path,
                            connection,
                            self.profile.root.clone(),
                        );
                    }
                    self.profile.outcome = "published";
                }
                PublishOutcome::Conflict { current_generation } => {
                    self.profile.outcome = "conflict";
                    return Err(ViewError::InvalidManifest(format!(
                        "publication base changed to {current_generation:?}"
                    )));
                }
            }
        }
        self.profile.finish();
        Ok(AssemblyReport {
            generation: self.report.generation.take(),
            manifest: self.report.manifest.take(),
            blob_puts: self.report.blob_puts,
            pending_paths: std::mem::take(&mut self.report.pending_paths),
            published: self.report.published,
        })
    }
}

impl Drop for PreparedAssembly {
    fn drop(&mut self) {
        if let Some((view, generation)) = &self.files {
            if view
                .current_generation()
                .is_ok_and(|current| current.as_deref() != Some(generation))
            {
                view.remove_generation_files(generation);
            }
        }
        // Keep the pin alive until generation cleanup has finished.
        self.pin.take();
    }
}

pub fn prepare_checkout(
    request: &AssemblyRequest,
    phase: &mut impl FnMut(&str) -> Result<()>,
) -> Result<PreparedAssembly> {
    let mut timing = super::profile::PublicationTiming::new(&request.project_root);
    // Every publisher (scheduler, migration import, tests) reaches this point
    // with the HEAD fingerprint it observed, so the read-path cache that
    // navigation and Tier-2 compare generations against is populated here
    // rather than only where the scheduler happened to compute it.
    super::cache_head_fingerprint(request.project_root.clone(), request.desired_head.clone());
    let mut profile = PublicationProfile::new(&request.project_root);
    profile.enter(0, phase)?;
    let view = ViewStore::open(&request.storage, &request.scope)?;
    let current_generation = view.current_generation()?;
    let base_pin = current_generation
        .as_deref()
        .map(|generation| crate::pins::QueryPin::acquire(view.view_dir(), generation))
        .transpose()
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    let previous = current_generation
        .as_deref()
        .map(|generation| view.load_manifest(generation))
        .transpose()?;
    timing.phase("previous_manifest");
    let head_started = Instant::now();
    let head = head_tree_entries(&request.project_root)
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    profile.head_ms = head_started.elapsed().as_millis();
    timing.phase("head_tree");
    let mut callgraph = BlobStore::open(
        &request.storage,
        request.family.clone(),
        BlobPlane::Callgraph,
    )
    .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    let semantic = BlobStore::open(
        &request.storage,
        request.family.clone(),
        BlobPlane::Semantic,
    )
    .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    let mut aliases = AliasStore::open(&request.storage, &request.family)
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;

    timing.phase("store_opens");
    let previous_entries = previous
        .as_ref()
        .map(|manifest| {
            manifest
                .entries()
                .map(|(path, entry)| (path.as_bytes().to_vec(), entry.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let rebuild_all = request.changed_paths.is_empty();
    let assembly_started = Instant::now();
    let mut blocking_paths = BTreeSet::new();
    let mut semantic_pending_paths = BTreeSet::new();
    let mut candidates = Vec::with_capacity(head.len());

    for tracked in head {
        let rel_path = RelPath::new(tracked.rel_path.clone())?;
        if !rebuild_all && !request.changed_paths.contains(&tracked.rel_path) {
            if let Some(entry) = previous_entries.get(&tracked.rel_path) {
                candidates.push(Candidate {
                    path: rel_path,
                    entry: entry.clone(),
                    key: None,
                    payload: None,
                    tracked: None,
                    source: None,
                });
                continue;
            }
        }

        match tracked.mode {
            GitMode::Gitlink => candidates.push(Candidate {
                path: rel_path,
                entry: ManifestEntry::Gitlink {
                    oid: tracked.git_oid.to_hex(),
                },
                key: None,
                payload: None,
                tracked: None,
                source: None,
            }),
            GitMode::Symlink => {
                let target = read_symlink_bytes(
                    &request
                        .project_root
                        .join(path_from_bytes(&tracked.rel_path)),
                )?;
                candidates.push(Candidate {
                    path: rel_path,
                    entry: ManifestEntry::Symlink {
                        target_bytes: ByteString::new(target),
                    },
                    key: None,
                    payload: None,
                    tracked: None,
                    source: None,
                });
            }
            GitMode::Regular { executable } => {
                let absolute = request
                    .project_root
                    .join(path_from_bytes(&tracked.rel_path));
                let source = fs::read(&absolute)?;
                let resolution_input = is_resolution_input(&tracked.rel_path);
                let language = if resolution_input {
                    Some("config".to_string())
                } else {
                    detect_language(&absolute)
                        .map(|language| format!("{language:?}").to_lowercase())
                };
                let (key, payload) = language
                    .as_deref()
                    .map(|language| {
                        let key = CallgraphKey::for_current(&source, language).full_key();
                        let key_hex = key.to_hex();
                        let callgraph_is_current = previous_entries
                            .get(&tracked.rel_path)
                            .is_some_and(|entry| {
                                manifest_entry_callgraph_key(entry) == Some(key_hex.as_str())
                            });
                        let payload = if callgraph_is_current {
                            None
                        } else {
                            missing_callgraph_payload(
                                &callgraph,
                                &key,
                                &source,
                                language,
                                resolution_input,
                            )?
                        };
                        Ok::<_, ViewError>((key, payload))
                    })
                    .transpose()?
                    .map_or((None, None), |(key, payload)| (Some(key), payload));
                let callgraph_key = key.as_ref().map(FullKey::to_hex);
                candidates.push(Candidate {
                    path: rel_path,
                    entry: ManifestEntry::Regular {
                        mode: if executable { 0o100755 } else { 0o100644 },
                        planes: RegularPlanes {
                            semantic: request.semantic_keys.get(&tracked.rel_path).cloned(),
                            callgraph: callgraph_key,
                        },
                        resolution_input,
                    },
                    key,
                    payload,
                    tracked: Some(tracked),
                    source: Some(source),
                });
            }
            GitMode::Other(_) => {
                blocking_paths.insert(tracked.rel_path);
            }
        }
    }

    timing.phase("candidate_assembly");
    profile.candidates = candidates.len();
    profile.assembly_ms = assembly_started.elapsed().as_millis();

    if request.require_semantic {
        for candidate in &candidates {
            let missing = matches!(
                &candidate.entry,
                ManifestEntry::Regular { planes, .. }
                    if planes.semantic.is_none()
                        && crate::semantic_index::is_semantic_indexed_extension(
                            &request
                                .project_root
                                .join(path_from_bytes(candidate.path.as_bytes()))
                        )
            );
            if missing {
                semantic_pending_paths.insert(candidate.path.as_bytes().to_vec());
            }
        }
    }

    let keys = candidates
        .iter()
        .filter_map(|candidate| candidate.key.clone())
        .collect::<Vec<_>>();
    let next_generation = next_generation(current_generation.as_deref(), &request.desired_head);
    profile.generation = next_generation.clone();
    let pin = AssemblyPin::create(
        view.view_dir(),
        request.family.clone(),
        request.scope.clone(),
        next_generation.clone(),
        &keys,
    )
    .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    let mut prepared = PreparedAssembly {
        report: AssemblyReport {
            generation: current_generation.clone(),
            manifest: previous.clone(),
            blob_puts: 0,
            pending_paths: BTreeSet::new(),
            published: false,
        },
        publication: None,
        files: Some((view.clone(), next_generation.clone())),
        derived_checkpoint: None,
        pin: Some(pin),
        _base_pin: base_pin,
        profile,
    };
    prepared.profile.enter(1, phase)?;
    timing.phase("assembly_pin");
    let mut blob_puts = 0;
    for candidate in &candidates {
        let (Some(key), Some(payload)) = (&candidate.key, &candidate.payload) else {
            continue;
        };
        if request.allow_blob_put {
            prepared
                .pin
                .as_mut()
                .expect("assembly pin")
                .renew_if_due()
                .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
            let put = callgraph
                .put(key, payload)
                .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
            blob_puts += usize::from(matches!(put.outcome, PutOutcome::Inserted));
            if let (Some(tracked), Some(source)) = (&candidate.tracked, &candidate.source) {
                aliases
                    .seed_proven_alias(tracked, source)
                    .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
            }
        } else if callgraph
            .get(key)
            .map_err(|error| ViewError::InvalidManifest(error.to_string()))?
            .is_none()
        {
            blocking_paths.insert(candidate.path.as_bytes().to_vec());
        }
    }

    timing.phase("blob_and_alias_puts");
    prepared.profile.blob_puts = blob_puts;
    prepared.profile.pending_paths = blocking_paths.len() + semantic_pending_paths.len();
    let mut status = PathStatusStore::open(view.view_dir())
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    for path in blocking_paths.iter().chain(&semantic_pending_paths) {
        status
            .mark_pending(
                path,
                if blocking_paths.contains(path) {
                    "shared callgraph blob unavailable"
                } else {
                    "shared semantic blob unavailable"
                },
                generation_number(&next_generation),
            )
            .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    }
    if !blocking_paths.is_empty() {
        prepared.profile.outcome = "pending";
        prepared.report.blob_puts = blob_puts;
        prepared.report.pending_paths = blocking_paths
            .union(&semantic_pending_paths)
            .cloned()
            .collect();
        return Ok(prepared);
    }
    for candidate in &candidates {
        if !semantic_pending_paths.contains(candidate.path.as_bytes()) {
            status
                .clear(candidate.path.as_bytes())
                .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
        }
    }
    timing.phase("path_status");
    prepared.report.pending_paths = semantic_pending_paths;

    let manifest = Manifest::new(
        candidates
            .into_iter()
            .map(|candidate| (candidate.path, candidate.entry)),
    )?;
    prepared.profile.semantic_fill = previous.as_ref().is_some_and(|base| {
        base.entries
            .iter()
            .map(|(path, entry)| (path, manifest_entry_callgraph_key(entry)))
            .eq(manifest
                .entries
                .iter()
                .map(|(path, entry)| (path, manifest_entry_callgraph_key(entry))))
            && base != &manifest
    });
    // A publication that reproduces the current manifest byte for byte is a
    // no-op: callers fire on triggers that often leave HEAD untouched (a
    // semantic refresh completing, a watcher batch of ignored edits), and each
    // redundant generation costs a full derived-database materialization and
    // a pointer swap. Keep the current generation and report nothing published.
    if current_generation
        .as_deref()
        .is_some_and(|generation| generation.ends_with(&request.desired_head))
        && previous.as_ref() == Some(&manifest)
    {
        prepared.profile.outcome = "no_op";
        prepared.report.manifest = None;
        prepared.report.blob_puts = blob_puts;
        return Ok(prepared);
    }
    prepared.profile.enter(2, phase)?;
    let reused_derived = previous
        .as_ref()
        .is_some_and(|base| super::materialization::manifest_callgraph_equivalent(base, &manifest))
        && current_generation
            .as_deref()
            .is_some_and(|base| view.derived_path(base).is_ok_and(|path| path.is_file()));
    if reused_derived {
        view.reuse_derived(
            &next_generation,
            current_generation.as_deref().expect("reused base"),
        )?;
    }
    prepared.profile.io.enter(super::io::Phase::Clone);
    let derived = view.derived_path(&next_generation)?;
    let mut cloned_base = false;
    if !reused_derived {
        let clone_started = Instant::now();
        if let Some(base) = current_generation.as_deref() {
            let base_path = view.derived_path(base)?;
            if base_path.is_file() {
                super::generation::clone_derived(&base_path, &derived)?;
                cloned_base = true;
            }
        }
        prepared.profile.derived_clone_ms = clone_started.elapsed().as_millis();
        // Keep one connection alive so SQLite does not checkpoint the committed WAL
        // when the materializer closes its writer before pointer publication.
        let derived_keeper = crate::db::file_identity::IdentityConnection::new(Connection::open(&derived)?, "views::assembly::assemble");
        derived_keeper.busy_timeout(std::time::Duration::from_secs(5))?;
        // Opening a handle or setting journal_mode alone does not attach its
        // pager to the WAL. Read the schema so closing the materializer is not
        // the last WAL connection and cannot checkpoint before publication.
        derived_keeper.pragma_update(None, "journal_mode", "WAL")?;
        derived_keeper.query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| {
            row.get::<_, i64>(0)
        })?;
        prepared.derived_checkpoint = Some((derived.clone(), derived_keeper));
        prepared.profile.io.enter(super::io::Phase::Materialize);
        let materialization_started = Instant::now();
        let derived_manifest = current_generation
            .as_deref()
            .filter(|_| cloned_base)
            .map(|base| {
                view.derived_owner(base)
                    .and_then(|owner| view.load_manifest(&owner))
            })
            .transpose()?;
        if let Some(base_manifest) = derived_manifest.as_ref() {
            let (stats, timings) = super::materialization::apply_manifest_diff_profiled(
                &derived,
                base_manifest,
                &manifest,
                callgraph.path(),
            )
            .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
            prepared.profile.materialization = timings;
            log::info!(
                "view manifest diff: generation={} stats={stats:?}",
                next_generation
            );
        } else {
            crate::callgraph_store::materialize_manifest_view_database(
                &derived,
                callgraph.path(),
                &manifest,
            )
            .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
        }
        prepared.profile.materialization_call_ms = materialization_started.elapsed().as_millis();
    }
    prepared.profile.io.enter(super::io::Phase::DerivedOther);
    let trigram = view.trigram_path(&next_generation)?;
    fs::write(&trigram, [])?;
    let artifacts = PublicationArtifacts {
        blob_databases: if reused_derived {
            vec![semantic.path().to_path_buf()]
        } else {
            vec![
                semantic.path().to_path_buf(),
                callgraph.path().to_path_buf(),
            ]
        },
        derived_database: derived.clone(),
        trigram_artifact: trigram.clone(),
        alias_database: aliases.path().to_path_buf(),
    };
    let closure = SqliteClosure {
        semantic: semantic.path().to_path_buf(),
        callgraph: callgraph.path().to_path_buf(),
        trigram,
        connections: Default::default(),
    };
    prepared.profile.io.enter(super::io::Phase::Closure);
    let closure_started = Instant::now();
    let publication = view.prepare_with_reused_derived(
        &PublicationRequest {
            generation: &next_generation,
            base_generation: current_generation.as_deref(),
            manifest: &manifest,
            artifacts,
            closure_requirements: ClosureRequirements::default(),
        },
        &closure,
        if reused_derived {
            previous.as_ref()
        } else {
            None
        },
    )?;
    prepared.profile.closure_ms = closure_started.elapsed().as_millis();
    prepared.profile.derived_bytes = fs::metadata(&derived)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    prepared.profile.enter(3, phase)?;
    prepared.publication = Some(publication);
    prepared.report = AssemblyReport {
        generation: Some(next_generation),
        manifest: Some(manifest),
        blob_puts,
        pending_paths: prepared.report.pending_paths.clone(),
        published: false,
    };
    Ok(prepared)
}

/// One profile follows the same boundaries as cancellation and health reporting.
/// It survives preparation so CAS time is included, and logs only after the
/// prepared generation leaves the actor barrier (including failed attempts).
struct PublicationProfile {
    root: PathBuf,
    io: super::io::PublicationIo,
    overlap: super::io::Overlap,
    concurrent_publications: u64,
    semantic_fill: bool,
    generation: String,
    outcome: &'static str,
    candidates: usize,
    blob_puts: usize,
    pending_paths: usize,
    head_ms: u128,
    assembly_ms: u128,
    pointer_ms: u128,
    phase_ms: [u128; 4],
    active_phase: Option<usize>,
    phase_started: Instant,
    started: Instant,
    total_ms: u128,
    derived_bytes: u64,
    derived_clone_ms: u128,
    materialization_call_ms: u128,
    closure_ms: u128,
    materialization: super::materialization::profile::PhaseTimings,
}

impl PublicationProfile {
    fn new(root: &Path) -> Self {
        let now = Instant::now();
        Self {
            root: root.to_owned(),
            io: super::io::PublicationIo::new(root),
            overlap: super::io::Overlap::new(true),
            concurrent_publications: 0,
            semantic_fill: false,
            generation: "none".into(),
            outcome: "cancelled_or_failed",
            candidates: 0,
            blob_puts: 0,
            pending_paths: 0,
            head_ms: 0,
            assembly_ms: 0,
            pointer_ms: 0,
            phase_ms: [0; 4],
            active_phase: Some(0),
            phase_started: now,
            started: now,
            total_ms: 0,
            derived_bytes: 0,
            derived_clone_ms: 0,
            materialization_call_ms: 0,
            closure_ms: 0,
            materialization: super::materialization::profile::PhaseTimings::default(),
        }
    }

    fn enter(&mut self, phase: usize, callback: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
        self.checkpoint();
        self.io.enter(
            [
                super::io::Phase::Manifest,
                super::io::Phase::Blobs,
                super::io::Phase::DerivedOther,
                super::io::Phase::Cas,
            ][phase],
        );
        self.active_phase = Some(phase);
        self.phase_started = Instant::now();
        callback(["manifest", "blobs", "derived", "cas"][phase])
    }

    fn checkpoint(&mut self) {
        if let Some(phase) = self.active_phase.take() {
            self.phase_ms[phase] += self.phase_started.elapsed().as_millis();
        }
    }

    fn finish(&mut self) {
        if self.active_phase.is_none() {
            return;
        }
        self.io.finish();
        self.concurrent_publications = self.overlap.finish();
        self.checkpoint();
        self.total_ms = self.started.elapsed().as_millis();
    }
}

impl Drop for PublicationProfile {
    fn drop(&mut self) {
        if self.active_phase.is_some() {
            self.finish();
        }
        log_publication_profile(self);
    }
}

fn log_publication_profile(profile: &PublicationProfile) {
    crate::slog_info!("{}", publication_profile_line(profile));
}

fn publication_profile_line(profile: &PublicationProfile) -> String {
    // Retain the existing drill fields alongside the shared phase names. The
    // pointer transaction is a subset of the cas phase, which also includes
    // waiting to acquire the actor barrier.
    format!(
        "index_event kind=view_publication plane=views root={} outcome={} candidates={} blob_puts={} pending_paths={} manifest_ms={} blobs_ms={} derived_ms={} cas_ms={} head_ms={} assembly_ms={} blob_ms={} materialize_ms={} derived_clone_ms={} materialization_call_ms={} closure_ms={} materialize_load_bindings_select_ms={} materialize_delete_rows_ms={} materialize_owned_blob_decode_insert_ms={} materialize_join_load_payloads_ms={} materialize_join_decode_bind_index_entries_ms={} materialize_join_index_surface_replay_ms={} materialize_join_decode_resolved_callers_ms={} materialize_join_resolve_record_ms={} materialize_join_dependency_union_ms={} materialize_selected_join_ms={} materialize_write_bindings_ms={} materialize_emit_refs_edges_ms={} materialize_commit_ms={} materialize_cleanup_memory_ms={} materialize_cleanup_connections_ms={} pointer_ms={} total_ms={} derived_bytes={} semantic_fill={} generation={} concurrent_publications={} {}",
        profile.root.display(), profile.outcome, profile.candidates, profile.blob_puts,
        profile.pending_paths, profile.phase_ms[0], profile.phase_ms[1],
        profile.phase_ms[2], profile.phase_ms[3], profile.head_ms, profile.assembly_ms,
        profile.phase_ms[1], profile.materialization_call_ms,
        profile.derived_clone_ms,
        profile.materialization_call_ms,
        profile.closure_ms,
        profile.materialization.load_bindings_select_ms,
        profile.materialization.delete_rows_ms,
        profile.materialization.owned_blob_decode_and_insert_ms,
        profile.materialization.join_load_payloads_ms,
        profile.materialization.join_decode_bind_index_entries_ms,
        profile.materialization.join_index_and_surface_replay_ms,
        profile.materialization.join_decode_resolved_callers_ms,
        profile.materialization.join_resolve_and_record_ms,
        profile.materialization.join_dependency_union_ms,
        profile.materialization.selected_join_ms,
        profile.materialization.write_bindings_ms,
        profile.materialization.emit_refs_edges_ms,
        profile.materialization.commit_ms,
        profile.materialization.cleanup_memory_ms,
        profile.materialization.cleanup_connections_ms,
        profile.pointer_ms, profile.total_ms, profile.derived_bytes,
        profile.semantic_fill, profile.generation, profile.concurrent_publications, profile.io.fields(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_profile_line_attributes_the_canonical_root() {
        let mut profile = PublicationProfile::new(Path::new("/checkout"));
        profile.outcome = "published";
        profile.phase_ms = [5, 6, 7, 8];
        profile.derived_clone_ms = 9;
        profile.materialization_call_ms = 10;
        profile.closure_ms = 11;
        profile.materialization = crate::views::materialization::profile::PhaseTimings {
            load_bindings_select_ms: 11,
            delete_rows_ms: 12,
            owned_blob_decode_and_insert_ms: 13,
            join_load_payloads_ms: 14,
            join_decode_bind_index_entries_ms: 15,
            join_index_and_surface_replay_ms: 16,
            join_decode_resolved_callers_ms: 17,
            join_resolve_and_record_ms: 18,
            join_dependency_union_ms: 19,
            selected_join_ms: 20,
            write_bindings_ms: 21,
            emit_refs_edges_ms: 22,
            commit_ms: 23,
            cleanup_memory_ms: 24,
            cleanup_connections_ms: 25,
            writes: Default::default(),
        };
        let line = publication_profile_line(&profile);
        assert!(line.contains("plane=views root=/checkout outcome=published"));
        assert!(line.contains("manifest_ms=5 blobs_ms=6 derived_ms=7 cas_ms=8"));
        assert!(line.contains("derived_clone_ms=9 materialization_call_ms=10 closure_ms=11"));
        assert!(line.contains("blob_ms=6 materialize_ms=10"));
        assert!(line.contains(
            "materialize_load_bindings_select_ms=11 materialize_delete_rows_ms=12 \
             materialize_owned_blob_decode_insert_ms=13 materialize_join_load_payloads_ms=14 \
             materialize_join_decode_bind_index_entries_ms=15 \
             materialize_join_index_surface_replay_ms=16 \
             materialize_join_decode_resolved_callers_ms=17 \
             materialize_join_resolve_record_ms=18 materialize_join_dependency_union_ms=19 \
             materialize_selected_join_ms=20 materialize_write_bindings_ms=21 \
             materialize_emit_refs_edges_ms=22 materialize_commit_ms=23 \
             materialize_cleanup_memory_ms=24 materialize_cleanup_connections_ms=25"
        ));
        assert!(line.contains("io_scope=process io_available="));
        assert!(line.contains("manifest_physical_bytes_written="));
        assert!(line.contains("closure_logical_bytes_written="));
        assert!(line.contains("total_bytes_read="));
        assert!(line.contains("semantic_fill=false generation=none concurrent_publications=0"));
        assert_eq!(line.matches("index_event kind=view_publication").count(), 1);
    }
}

fn manifest_entry_callgraph_key(entry: &ManifestEntry) -> Option<&str> {
    match entry {
        ManifestEntry::Regular { planes, .. } => planes.callgraph.as_deref(),
        ManifestEntry::Synthetic { planes, .. } => Some(&planes.callgraph),
        ManifestEntry::Symlink { .. } | ManifestEntry::Gitlink { .. } => None,
    }
}

pub(super) fn is_resolution_input(path: &[u8]) -> bool {
    let name = path.rsplit(|byte| *byte == b'/').next().unwrap_or(path);
    name == b"package.json"
        || name == b"Cargo.toml"
        || name == b".gitignore"
        || name.starts_with(b"tsconfig") && name.ends_with(b".json")
}

fn next_generation(current: Option<&str>, desired_head: &str) -> String {
    let generation = current
        .map(generation_number)
        .unwrap_or(0)
        .saturating_add(1);
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{generation}-{}-{nanos}-{serial}-{desired_head}",
        std::process::id()
    )
}

fn generation_number(generation: &str) -> u64 {
    generation
        .split_once('-')
        .and_then(|(number, _)| number.parse().ok())
        .unwrap_or(0)
}

struct SqliteClosure {
    semantic: PathBuf,
    callgraph: PathBuf,
    trigram: PathBuf,
    // Lazy handles preserve probes of empty planes and malformed keys without
    // opening their databases. Valid keys share a handle for this closure only.
    connections: std::cell::RefCell<BTreeMap<bool, crate::db::lifecycle::TrackedConnection>>,
}

impl PublicationClosure for SqliteClosure {
    fn contains_blob(&self, plane: ArtifactPlane, full_key: &str) -> Result<bool> {
        let Some(key) = decode_hex(full_key) else {
            return Ok(false);
        };
        let path = match plane {
            ArtifactPlane::Semantic => &self.semantic,
            ArtifactPlane::Callgraph => &self.callgraph,
        };
        let mut connections = self.connections.borrow_mut();
        let semantic = plane == ArtifactPlane::Semantic;
        if let std::collections::btree_map::Entry::Vacant(entry) = connections.entry(semantic) {
            entry.insert(crate::db::lifecycle::TrackedConnection::open(
                path,
                crate::db::lifecycle::SqliteStore::BlobStore,
            )?);
        }
        let present = connections[&semantic]
            .prepare_cached(
                "SELECT 1 FROM blob_payloads INDEXED BY blob_membership WHERE full_key = ?1",
            )?
            .query_row([key], |_| Ok(()))
            .optional()?
            .is_some();
        Ok(present)
    }

    fn probe_blobs(&self, keys: &[(ArtifactPlane, &str)]) -> Result<()> {
        let mut present = BTreeSet::new();
        for (plane_id, plane) in [ArtifactPlane::Semantic, ArtifactPlane::Callgraph]
            .into_iter()
            .enumerate()
        {
            let wanted = keys
                .iter()
                .filter(|(p, _)| *p == plane)
                .map(|(_, key)| *key)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            for chunk in wanted.chunks(500) {
                // Sorted key batches follow the blob index rather than manifest
                // path order. Keep the tracked plane handle across all batches.
                let Some(first_valid) = chunk.iter().find(|key| decode_hex(key).is_some()) else {
                    continue;
                };
                self.contains_blob(plane, first_valid)?;
                let connections = self.connections.borrow();
                let Some(connection) = connections.get(&(plane == ArtifactPlane::Semantic)) else {
                    continue;
                };
                let decoded = chunk
                    .iter()
                    .filter_map(|key| decode_hex(key))
                    .collect::<Vec<_>>();
                if decoded.is_empty() {
                    continue;
                }
                let sql = membership_query(decoded.len());
                let mut statement = connection.prepare_cached(&sql)?;
                for key in statement.query_map(rusqlite::params_from_iter(&decoded), |row| {
                    row.get::<_, Vec<u8>>(0)
                })? {
                    present.insert((plane_id, key?));
                }
            }
        }
        // Return the first missing key in manifest order, even though membership
        // reads are grouped by plane and sorted for index locality.
        for &(plane, key) in keys {
            let plane_id = usize::from(plane == ArtifactPlane::Callgraph);
            if decode_hex(key).is_none_or(|key| !present.contains(&(plane_id, key))) {
                return Err(ViewError::MissingBlob {
                    plane,
                    key: key.to_owned(),
                });
            }
        }
        Ok(())
    }

    fn trigram_is_present(&self) -> Result<bool> {
        Ok(self.trigram.is_file())
    }

    fn contains_alias(&self, _git_oid: &str) -> Result<bool> {
        Ok(true)
    }
}

fn missing_callgraph_payload(
    store: &BlobStore,
    key: &FullKey,
    source: &[u8],
    language: &str,
    resolution_input: bool,
) -> Result<Option<Vec<u8>>> {
    // A branch return can name content absent from the previous manifest but
    // already stored by an earlier generation. Validate that payload before
    // paying for extraction, serialization, and an immutable no-op put.
    if store
        .get(key)
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?
        .is_some()
    {
        return Ok(None);
    }
    let blob = if resolution_input {
        CallgraphBlob::config(source.to_vec(), CALLGRAPH_PRODUCER_VERSION)
    } else {
        CallgraphBlob::extract(
            std::str::from_utf8(source)
                .map_err(|error| ViewError::InvalidManifest(error.to_string()))?,
            language,
            CALLGRAPH_PRODUCER_VERSION,
        )
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?
    };
    Ok(Some(blob.to_bytes().map_err(|error| {
        ViewError::InvalidManifest(error.to_string())
    })?))
}

#[cfg(test)]
mod reuse_tests {
    use super::*;

    #[test]
    fn views_corrupt_cached_payload_is_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = BlobStore::open(dir.path(), "corrupt", BlobPlane::Callgraph).unwrap();
        let source = b"export function target() {}";
        let key = CallgraphKey::for_current(source, "typescript").full_key();
        let payload = missing_callgraph_payload(&store, &key, source, "typescript", false)
            .unwrap()
            .unwrap();
        store.put(&key, &payload).unwrap();
        Connection::open(store.path())
            .unwrap()
            .execute("UPDATE blob_payloads SET payload_digest = zeroblob(32)", [])
            .unwrap();
        assert!(
            missing_callgraph_payload(&store, &key, source, "typescript", false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn views_cached_callgraph_payload_does_not_extract_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = BlobStore::open(dir.path(), "reuse", BlobPlane::Callgraph).unwrap();
        let source = b"export function target() {}";
        let key = CallgraphKey::for_current(source, "typescript").full_key();
        let payload = missing_callgraph_payload(&store, &key, source, "typescript", false)
            .unwrap()
            .unwrap();
        store.put(&key, &payload).unwrap();
        assert!(
            missing_callgraph_payload(&store, &key, source, "typescript", false)
                .unwrap()
                .is_none(),
            "cached content must not be extracted or put again"
        );
    }
}

fn membership_query(count: usize) -> String {
    format!(
        "SELECT full_key FROM blob_payloads INDEXED BY blob_membership WHERE full_key IN ({})",
        vec!["?"; count].join(",")
    )
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn read_symlink_bytes(path: &Path) -> Result<Vec<u8>> {
    let target = fs::read_link(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(target.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        Ok(target.to_string_lossy().as_bytes().to_vec())
    }
}

#[cfg(test)]
#[path = "closure_connection_tests.rs"]
mod closure_connection_tests;

#[cfg(test)]
#[path = "semantic_fill_tests.rs"]
mod semantic_fill_tests;
