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
            let pointer_started = Instant::now();
            let outcome = publication.commit();
            self.profile.pointer_ms = pointer_started.elapsed().as_millis();
            match outcome? {
                PublishOutcome::Published => {
                    self.report.published = true;
                    self.files = None;
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
    let head_started = Instant::now();
    let head = head_tree_entries(&request.project_root)
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    profile.head_ms = head_started.elapsed().as_millis();
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
    let mut pending_paths = BTreeSet::new();
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
                        let blob = if resolution_input {
                            CallgraphBlob::config(source.clone(), CALLGRAPH_PRODUCER_VERSION)
                        } else {
                            CallgraphBlob::extract(
                                std::str::from_utf8(&source).map_err(|error| {
                                    ViewError::InvalidManifest(error.to_string())
                                })?,
                                language,
                                CALLGRAPH_PRODUCER_VERSION,
                            )
                            .map_err(|error| ViewError::InvalidManifest(error.to_string()))?
                        };
                        Ok::<_, ViewError>((
                            key,
                            blob.to_bytes()
                                .map_err(|error| ViewError::InvalidManifest(error.to_string()))?,
                        ))
                    })
                    .transpose()?
                    .map_or((None, None), |(key, payload)| (Some(key), Some(payload)));
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
                pending_paths.insert(tracked.rel_path);
            }
        }
    }

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
                pending_paths.insert(candidate.path.as_bytes().to_vec());
            }
        }
    }

    let keys = candidates
        .iter()
        .filter_map(|candidate| candidate.key.clone())
        .collect::<Vec<_>>();
    let next_generation = next_generation(current_generation.as_deref(), &request.desired_head);
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
        pin: Some(pin),
        _base_pin: base_pin,
        profile,
    };
    prepared.profile.enter(1, phase)?;
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
            pending_paths.insert(candidate.path.as_bytes().to_vec());
        }
    }

    prepared.profile.blob_puts = blob_puts;
    prepared.profile.pending_paths = pending_paths.len();
    let mut status = PathStatusStore::open(view.view_dir())
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    if !pending_paths.is_empty() {
        for path in &pending_paths {
            status
                .mark_pending(
                    path,
                    "shared blob unavailable",
                    generation_number(&next_generation),
                )
                .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
        }
        prepared.profile.outcome = "pending";
        prepared.report.blob_puts = blob_puts;
        prepared.report.pending_paths = pending_paths;
        return Ok(prepared);
    }
    for candidate in &candidates {
        status
            .clear(candidate.path.as_bytes())
            .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    }

    let manifest = Manifest::new(
        candidates
            .into_iter()
            .map(|candidate| (candidate.path, candidate.entry)),
    )?;
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
    let derived = view.derived_path(&next_generation)?;
    if let Some(base) = current_generation.as_deref() {
        let base_path = view.derived_path(base)?;
        if base_path.is_file() {
            super::generation::clone_derived(&base_path, &derived)?;
        }
    }
    crate::callgraph_store::materialize_manifest_view_database(
        &derived,
        callgraph.path(),
        &manifest,
    )
    .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    let trigram = view.trigram_path(&next_generation)?;
    fs::write(&trigram, [])?;
    let artifacts = PublicationArtifacts {
        blob_databases: vec![
            semantic.path().to_path_buf(),
            callgraph.path().to_path_buf(),
        ],
        derived_database: derived.clone(),
        trigram_artifact: trigram.clone(),
        alias_database: aliases.path().to_path_buf(),
    };
    let closure = SqliteClosure {
        semantic: semantic.path().to_path_buf(),
        callgraph: callgraph.path().to_path_buf(),
        trigram,
    };
    let publication = view.prepare_with_observer(
        &PublicationRequest {
            generation: &next_generation,
            base_generation: current_generation.as_deref(),
            manifest: &manifest,
            artifacts,
            closure_requirements: ClosureRequirements::default(),
        },
        &closure,
        None,
    )?;
    prepared.profile.derived_bytes = fs::metadata(&derived)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    prepared.profile.enter(3, phase)?;
    prepared.publication = Some(publication);
    prepared.report = AssemblyReport {
        generation: Some(next_generation),
        manifest: Some(manifest),
        blob_puts,
        pending_paths,
        published: false,
    };
    Ok(prepared)
}

/// One profile follows the same boundaries as cancellation and health reporting.
/// It survives preparation so CAS time is included, and logs only after the
/// prepared generation leaves the actor barrier (including failed attempts).
struct PublicationProfile {
    root: PathBuf,
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
}

impl PublicationProfile {
    fn new(root: &Path) -> Self {
        let now = Instant::now();
        Self {
            root: root.to_owned(),
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
        }
    }

    fn enter(&mut self, phase: usize, callback: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
        self.checkpoint();
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
        "index_event kind=view_publication plane=views root={} outcome={} candidates={} blob_puts={} pending_paths={} manifest_ms={} blobs_ms={} derived_ms={} cas_ms={} head_ms={} assembly_ms={} blob_ms={} materialize_ms={} pointer_ms={} total_ms={} derived_bytes={}",
        profile.root.display(), profile.outcome, profile.candidates, profile.blob_puts,
        profile.pending_paths, profile.phase_ms[0], profile.phase_ms[1],
        profile.phase_ms[2], profile.phase_ms[3], profile.head_ms, profile.assembly_ms,
        profile.phase_ms[1], profile.phase_ms[2], profile.pointer_ms,
        profile.total_ms, profile.derived_bytes,
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
        let line = publication_profile_line(&profile);
        assert!(line.contains("plane=views root=/checkout outcome=published"));
        assert!(line.contains("manifest_ms=5 blobs_ms=6 derived_ms=7 cas_ms=8"));
        assert_eq!(line.matches("index_event kind=view_publication").count(), 1);
    }
}

fn is_resolution_input(path: &[u8]) -> bool {
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
        Ok(Connection::open(path)?
            .query_row(
                "SELECT 1 FROM blob_payloads WHERE full_key = ?1",
                [key],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    fn trigram_is_present(&self) -> Result<bool> {
        Ok(self.trigram.is_file())
    }

    fn contains_alias(&self, _git_oid: &str) -> Result<bool> {
        Ok(true)
    }
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
