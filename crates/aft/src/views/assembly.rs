use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

use crate::alias::{head_tree_entries, AliasStore, GitMode};
use crate::blob_store::{
    BlobPlane, BlobStore, CallgraphKey, FullKey, PutOutcome, CALLGRAPH_PRODUCER_VERSION,
};
use crate::callgraph_store::join::{
    CallgraphBlob, JoinResult, ManifestBlobReader, ManifestJoinError,
};
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
    let view = ViewStore::open(&request.storage, &request.scope)?;
    let current_generation = view.current_generation()?;
    let previous = current_generation
        .as_deref()
        .map(|generation| view.load_manifest(generation))
        .transpose()?;
    let head = head_tree_entries(&request.project_root)
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
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
                            semantic: previous_entries.get(&tracked.rel_path).and_then(|entry| {
                                if let ManifestEntry::Regular { planes, .. } = entry {
                                    planes.semantic.clone()
                                } else {
                                    None
                                }
                            }),
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

    let keys = candidates
        .iter()
        .filter_map(|candidate| candidate.key.clone())
        .collect::<Vec<_>>();
    let next_generation = next_generation(current_generation.as_deref(), &request.desired_head);
    let mut pin = AssemblyPin::create(
        view.view_dir(),
        request.family.clone(),
        request.scope.clone(),
        next_generation.clone(),
        &keys,
    )
    .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    let mut blob_puts = 0;
    for candidate in &candidates {
        let (Some(key), Some(payload)) = (&candidate.key, &candidate.payload) else {
            continue;
        };
        if request.allow_blob_put {
            pin.renew_if_due()
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
        return Ok(AssemblyReport {
            generation: current_generation,
            manifest: previous,
            blob_puts,
            pending_paths,
            published: false,
        });
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
    let reader = SqliteCallgraphReader(callgraph.path().to_path_buf());
    let joined = JoinResult::from_manifest(&manifest, &reader)
        .map_err(|error| ViewError::InvalidManifest(error.to_string()))?;
    write_derived(status.path(), &joined.canonical_serialization())?;
    let trigram = view.view_dir().join("trigram.bin");
    fs::write(&trigram, [])?;
    let artifacts = PublicationArtifacts {
        blob_databases: vec![
            semantic.path().to_path_buf(),
            callgraph.path().to_path_buf(),
        ],
        derived_database: status.path().to_path_buf(),
        trigram_artifact: trigram.clone(),
        alias_database: aliases.path().to_path_buf(),
    };
    let closure = SqliteClosure {
        semantic: semantic.path().to_path_buf(),
        callgraph: callgraph.path().to_path_buf(),
        trigram,
    };
    let publication = view.publish(
        &PublicationRequest {
            generation: &next_generation,
            base_generation: current_generation.as_deref(),
            manifest: &manifest,
            artifacts,
            closure_requirements: ClosureRequirements::default(),
        },
        &closure,
    )?;
    pin.release();
    let published = matches!(publication, PublishOutcome::Published);
    let generation = match publication {
        PublishOutcome::Published => Some(next_generation),
        PublishOutcome::Conflict { current_generation } => current_generation,
    };
    Ok(AssemblyReport {
        generation,
        manifest: published.then_some(manifest),
        blob_puts,
        pending_paths,
        published,
    })
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
    format!("{generation}-{desired_head}")
}

fn generation_number(generation: &str) -> u64 {
    generation
        .split_once('-')
        .and_then(|(number, _)| number.parse().ok())
        .unwrap_or(0)
}

fn write_derived(path: &Path, rows: &[u8]) -> Result<()> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS callgraph_join (singleton INTEGER PRIMARY KEY, rows BLOB NOT NULL);",
    )?;
    connection.execute(
        "INSERT INTO callgraph_join(singleton, rows) VALUES (1, ?1)
         ON CONFLICT(singleton) DO UPDATE SET rows = excluded.rows",
        [rows],
    )?;
    Ok(())
}

struct SqliteCallgraphReader(PathBuf);

impl ManifestBlobReader for SqliteCallgraphReader {
    fn read_callgraph_blob(
        &self,
        full_key: &str,
    ) -> std::result::Result<Option<Vec<u8>>, ManifestJoinError> {
        let Some(key) = decode_hex(full_key) else {
            return Ok(None);
        };
        Connection::open(&self.0)
            .map_err(|error| ManifestJoinError::InvalidBlob(error.to_string()))?
            .query_row(
                "SELECT payload FROM blob_payloads WHERE full_key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ManifestJoinError::InvalidBlob(error.to_string()))
    }
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
