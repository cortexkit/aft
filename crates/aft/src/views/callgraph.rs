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
use super::manifest_v2::EntryV2;
use super::readiness::{plane_readiness, FillMap, PlaneReadiness, PlaneState};
use super::snapshot::{ComposedGeneration, DiskState, LiveEntry, Snapshot};
use super::{Manifest, ManifestEntry, RegularPlanes, RelPath};

/// Bump when extracted evidence, its encoding or interpretation changes. Version
/// 9 omits the extraction-only AST index from stored payloads; facts are unchanged.
/// Version 10 stores the parser's corrected symbols: Go type aliases, separate
/// ranges for specs in a grouped `type (...)` block, and non-inverted ranges for
/// HTML headings that share a row.
pub const PRODUCER: &str = "ruled-callgraph-v10-go-alias-and-heading-ranges";

#[derive(Clone, Debug)]
pub struct CallgraphAttachment {
    pub blob: Arc<CallgraphBlob>,
    pub key: FamilyKey,
}

/// Markdown has outline headings, not callable definitions or call references.
/// It remains a manifest member for other planes, but needs no callgraph blob.
pub(crate) fn source_language(path: &Path) -> Option<crate::parser::LangId> {
    crate::parser::detect_language(path)
        .filter(|language| *language != crate::parser::LangId::Markdown)
}

/// Whether a member needs a callgraph blob: a source file in a callgraph
/// language, or resolver configuration such as `package.json`.
pub(crate) fn applies_to(path: &RelPath) -> bool {
    super::assembly::is_resolution_input(path.as_bytes())
        || std::str::from_utf8(path.as_bytes())
            .ok()
            .is_some_and(|p| source_language(Path::new(p)).is_some())
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

/// Reads callgraph blobs from the family's v3 callgraph plane store.
pub struct V3BlobReader(pub crate::blob_store::v3::PlaneReader);
impl ManifestBlobReader for V3BlobReader {
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
pub fn project_manifest(
    manifest: &super::snapshot::ComposedGeneration,
) -> Result<Manifest, PlaneError> {
    if manifest.header().producers.callgraph != PRODUCER {
        return Err(error("incompatible callgraph producer"));
    }
    let mut entries = Vec::new();
    for (path, entry) in manifest.entries() {
        let projected = match entry {
            EntryV2::Regular {
                mode,
                resolution_input,
                ..
            } => {
                let key = match manifest.plane_readiness(path, FamilyPlane::Callgraph) {
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
    manifest: &super::snapshot::ComposedGeneration,
    reader: &impl ManifestBlobReader,
) -> Result<(), PlaneError> {
    let projected = project_manifest(manifest)?;
    super::materialization::materialize_from_blob_reader(database, &projected, reader)
        .map_err(error)
}

/// The checkout root recorded for a v3 member, read without any lock.
fn v3_member_root(route: &super::snapshot::V3Route) -> Result<PathBuf, PlaneError> {
    use rusqlite::OptionalExtension;
    let root: Option<Option<Vec<u8>>> =
        crate::blob_store::v3::LedgerReader::open_existing(&route.storage, &route.family)
            .and_then(|ledger| {
                ledger.read(|c| {
                    Ok(c.query_row(
                        "SELECT root FROM members WHERE scope=?1",
                        [&route.scope],
                        |r| r.get(0),
                    )
                    .optional()?)
                })
            })
            .map_err(error)?;
    let bytes = root
        .flatten()
        .ok_or_else(|| error("v3 member root cannot be bound"))?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|_| error("v3 member root is not UTF-8"))
    }
}

/// Holding a reader keeps exactly its generation pinned, including after a swap.
pub struct PinnedReader {
    pub store: Arc<ReadonlyCallGraphStore>,
    pub generation: Arc<ComposedGeneration>,
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
        let reader = self
            .readers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(
                access.scope().to_string(),
                snapshot.generation().resident_key(),
            ))
            .cloned();
        match reader {
            Some(reader) => Ok(reader),
            None => {
                // A refused keyless generation will never have a resident reader.
                // Preserve that refusal instead of making it look like a build.
                queryable_manifest(snapshot.generation())?;
                Err(error("pinned callgraph reader is not resident"))
            }
        }
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
        applies_to(path)
    }
    fn open_generation(
        &self,
        access: &ViewAccess,
        generation: &Arc<ComposedGeneration>,
    ) -> Result<(), PlaneError> {
        queryable_manifest(generation)?;
        let route = access.route(generation);
        let (root, dir): (PathBuf, PathBuf) = match (access, route) {
            // A v3 generation's derived graph lives in its v3 member
            // directory; the checkout root binds rendering as on v2.
            (ViewAccess::Owner(view), super::contracts::StorageRoute::V3(route)) => {
                (view.root().to_path_buf(), route.member_dir())
            }
            (ViewAccess::Reader { .. }, super::contracts::StorageRoute::V3(route)) => {
                (v3_member_root(&route)?, route.member_dir())
            }
            (ViewAccess::Owner(view), _) => {
                (view.root().to_path_buf(), view.view_dir().to_path_buf())
            }
            (
                ViewAccess::Reader {
                    registration,
                    scope,
                },
                _,
            ) => {
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
        let retired = self
            .readers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                (access.scope().into(), generation.resident_key()),
                Arc::new(PinnedReader {
                    store: Arc::new(store.retain_generation(generation.clone())),
                    generation: generation.clone(),
                }),
            );
        drop(retired);
        Ok(())
    }
    fn release_generation(&self, access: &ViewAccess, generation: &str) {
        let retired = self
            .readers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(access.scope().into(), generation.into()));
        drop(retired);
    }
    fn readiness(&self, access: &ViewAccess, snapshot: &Snapshot) -> PlaneReadiness {
        if !self
            .readers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(
                access.scope().to_owned(),
                snapshot.generation().resident_key(),
            ))
            .is_some_and(|reader| {
                reader.generation.resident_key() == snapshot.generation().resident_key()
            })
        {
            return PlaneReadiness::Building;
        }
        let readiness = plane_readiness(
            FamilyPlane::Callgraph,
            Some(snapshot.generation()),
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
fn queryable_manifest(
    manifest: &super::snapshot::ComposedGeneration,
) -> Result<Manifest, PlaneError> {
    let projected = project_manifest(manifest)?;
    // A disabled plane still has a valid empty derived database. It must not
    // answer navigation or reachability questions as though a graph was built.
    if super::assembly::manifest_lacks_callgraph(&projected) {
        return Err(error(super::read::CALLGRAPH_DISABLED));
    }
    Ok(projected)
}
fn error(reason: impl std::fmt::Display) -> PlaneError {
    PlaneError {
        plane: Some(FamilyPlane::Callgraph),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_not_a_callgraph_input_but_resolution_config_is() {
        let plane = CallgraphPlane::default();
        for document in [
            "notes.md",
            "notes.markdown",
            "notes.mdx",
            "notes.qmd",
            "notes.Rmd",
        ] {
            let path = RelPath::from_os_path(Path::new(document)).unwrap();
            assert!(
                !plane.applies_to(&path),
                "{document} requires a callgraph blob"
            );
        }
        for input in [
            "index.ts",
            "src/lib.rs",
            "package.json",
            "tsconfig.json",
            "Cargo.toml",
            ".gitignore",
        ] {
            let path = RelPath::from_os_path(Path::new(input)).unwrap();
            assert!(plane.applies_to(&path), "{input} lost its callgraph input");
        }
    }
}
