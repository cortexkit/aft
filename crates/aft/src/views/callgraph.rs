//! Callgraph plane APIs, opt-in until runtime cutover.
//!
//! Composite FirstLoadDriver sequence:
//! 1. `reconcile`: strictly read/hash current members and call `attach` with the
//!    same bytes used by the other planes; put immutable blobs under protected keys.
//! 2. `build_own_generation`: project the v2 manifest through `materialize` into
//!    the private derived path, then compare-and-swap the publication pointer
//!    while a pin prevents reclamation. Copying another view's derived graph stays
//!    disabled until its logical rows are proven equal to an independent rebuild.
//! 3. `install`: call `open_generation` for the verified pinned publication, then
//!    atomically swap its reader and snapshot under the root lock only if the
//!    reconcile revision still matches. Opening a reader alone is not installation.
//! 4. Queries using callgraph, inspect or dead-code must first wait through
//!    QueryWait for CALLGRAPH_QUERY_WAIT (the bounded three-second wait from
//!    `views::contracts`). Use its final installed snapshot and
//!    disclose timeout gap paths; never substitute legacy/newer mutable state.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::blob_store::v2::{parse_hex32, FamilyKey, FamilyPlane, FamilyStoreReader};
use crate::callgraph_store::join::{CallgraphBlob, ManifestBlobReader, ManifestJoinError};
use crate::callgraph_store::ReadonlyCallGraphStore;

use super::contracts::{PlaneAdapter, PlaneError, ViewAccess};
use super::manifest_v2::{EntryV2, ManifestV2};
use super::readiness::{plane_readiness, FillMap, PlaneReadiness, PlaneState};
use super::snapshot::{DiskState, LiveEntry, OpenGeneration, Snapshot};
use super::{Manifest, ManifestEntry, RegularPlanes, RelPath};

/// Bump when receiver hints or their interpretation changes. Legacy extraction
/// keys are not compatible with this producer and cannot silently seed its graph.
pub const PRODUCER: &str = "ruled-callgraph-v1";

#[derive(Clone, Debug)]
pub struct CallgraphAttachment {
    pub blob: Arc<CallgraphBlob>,
    pub key: FamilyKey,
}

/// Attach evidence computed from exactly the caller's already-read byte buffer.
/// No disk access, blob put, publication or membership decision occurs here.
pub fn attach(
    entry: &mut LiveEntry,
    source: &[u8],
    language: &str,
) -> Result<CallgraphAttachment, PlaneError> {
    if entry.disk != DiskState::of_bytes(source) {
        return Err(error("attachment bytes differ from live entry"));
    }
    let blob = if language == "config" {
        CallgraphBlob::config(source.to_vec(), PRODUCER)
    } else {
        CallgraphBlob::extract(
            std::str::from_utf8(source).map_err(error)?,
            language,
            PRODUCER,
        )
        .map_err(error)?
    };
    let key = crate::blob_store::CallgraphKey::from_bytes(source, language, PRODUCER).full_key();
    let attachment = CallgraphAttachment {
        blob: Arc::new(blob),
        key: FamilyKey::from(&key),
    };
    entry
        .attachments
        .insert(FamilyPlane::Callgraph, Arc::new(attachment.clone()));
    Ok(attachment)
}

pub struct BlobReader(pub FamilyStoreReader);
impl ManifestBlobReader for BlobReader {
    fn read_callgraph_blob(&self, key: &str) -> Result<Option<Vec<u8>>, ManifestJoinError> {
        let bytes = parse_hex32(key)
            .ok_or_else(|| ManifestJoinError::InvalidBlob("invalid callgraph key".into()))?;
        self.0
            .get(&FamilyKey::new(FamilyPlane::Callgraph, bytes))
            .map_err(|e| ManifestJoinError::InvalidBlob(e.to_string()))
    }
}

/// Project membership and compatible ready keys, not checkout paths/configuration.
/// Missing/failed callgraph members refuse construction rather than falsely ready.
pub fn project_manifest(manifest: &ManifestV2) -> Result<Manifest, PlaneError> {
    if manifest.header().producers.callgraph != PRODUCER {
        return Err(error("incompatible callgraph producer"));
    }
    let mut entries = Vec::new();
    for (path, entry) in manifest.entries() {
        let projected = match entry {
            EntryV2::Regular {
                mode,
                resolution_input,
                planes,
                ..
            } => {
                let key = match &planes.callgraph {
                    Some(PlaneState::Ready { key }) => Some(key.clone()),
                    None => None,
                    Some(_) => {
                        return Err(error(format!("callgraph member is not ready: {path:?}")))
                    }
                };
                ManifestEntry::Regular {
                    mode: *mode,
                    planes: RegularPlanes {
                        callgraph: key,
                        semantic: None,
                    },
                    resolution_input: *resolution_input,
                }
            }
            EntryV2::Symlink { target_bytes } => ManifestEntry::Symlink {
                target_bytes: target_bytes.clone(),
            },
            EntryV2::Gitlink { oid } => ManifestEntry::Gitlink { oid: oid.clone() },
            EntryV2::Synthetic { .. } => {
                return Err(error(
                    "synthetic v2 callgraph input needs a regular immutable blob",
                ))
            }
        };
        entries.push((path.clone(), projected));
    }
    Manifest::new(entries).map_err(error)
}

pub fn materialize(
    database: &Path,
    manifest: &ManifestV2,
    reader: &impl ManifestBlobReader,
) -> Result<(), PlaneError> {
    let projected = project_manifest(manifest)?;
    super::materialization::materialize_from_blob_reader(database, &projected, reader)
        .map_err(error)
}

/// Holding a reader keeps exactly its generation pinned, including after a swap.
pub struct PinnedReader {
    pub store: Arc<ReadonlyCallGraphStore>,
    pub generation: Arc<OpenGeneration>,
}
#[derive(Default)]
pub struct CallgraphPlane {
    readers: RwLock<BTreeMap<(String, String), Arc<PinnedReader>>>,
}
impl CallgraphPlane {
    pub fn reader(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
    ) -> Result<Arc<PinnedReader>, PlaneError> {
        self.readers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(
                access.scope().to_string(),
                snapshot.generation().name().to_string(),
            ))
            .cloned()
            .ok_or_else(|| error("pinned callgraph reader is not resident"))
    }
}
impl PlaneAdapter for CallgraphPlane {
    fn plane(&self) -> FamilyPlane {
        FamilyPlane::Callgraph
    }
    fn producer(&self) -> String {
        PRODUCER.into()
    }
    fn applies_to(&self, path: &RelPath) -> bool {
        super::assembly::is_resolution_input(path.as_bytes())
            || std::str::from_utf8(path.as_bytes())
                .ok()
                .is_some_and(|p| crate::parser::detect_language(Path::new(p)).is_some())
    }
    fn open_generation(
        &self,
        access: &ViewAccess,
        generation: &Arc<OpenGeneration>,
    ) -> Result<(), PlaneError> {
        project_manifest(generation.manifest())?;
        let (root, dir): (PathBuf, PathBuf) = match access {
            ViewAccess::Owner(view) => (view.root().to_path_buf(), view.view_dir().to_path_buf()),
            ViewAccess::Reader {
                registration,
                scope,
            } => {
                let member = registration
                    .members()
                    .map_err(error)?
                    .into_iter()
                    .find(|m| &m.scope == scope)
                    .ok_or_else(|| error("member not registered"))?;
                (
                    member
                        .root
                        .ok_or_else(|| error("member root cannot be bound"))?,
                    member.view_dir,
                )
            }
        };
        let store = ReadonlyCallGraphStore::open_pinned_derived(
            root,
            access.family().into(),
            dir,
            generation.name(),
        )
        .map_err(error)?;
        self.readers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                (access.scope().into(), generation.name().into()),
                Arc::new(PinnedReader {
                    store: Arc::new(store.retain_generation(generation.clone())),
                    generation: generation.clone(),
                }),
            );
        Ok(())
    }
    fn release_generation(&self, access: &ViewAccess, generation: &str) {
        self.readers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(access.scope().into(), generation.into()));
    }
    fn readiness(&self, access: &ViewAccess, snapshot: &Snapshot) -> PlaneReadiness {
        if !self
            .readers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(
                access.scope().to_owned(),
                snapshot.generation().name().to_owned(),
            ))
            .is_some_and(|reader| Arc::ptr_eq(&reader.generation, snapshot.generation()))
        {
            return PlaneReadiness::Building;
        }
        let readiness = plane_readiness(
            FamilyPlane::Callgraph,
            Some(snapshot.generation().manifest()),
            snapshot,
            &FillMap::default(),
            &|path| self.applies_to(path),
        );
        if snapshot.pending_intent().next().is_some() {
            PlaneReadiness::Building
        } else {
            readiness
        }
    }
}
fn error(reason: impl std::fmt::Display) -> PlaneError {
    PlaneError {
        plane: FamilyPlane::Callgraph,
        reason: reason.to_string(),
    }
}
