//! The common reader for an already selected, pinned publication.

use std::path::PathBuf;
use std::sync::Arc;

use crate::callgraph_store::{ReadonlyCallGraphStore, Result};
use crate::pins::QueryPin;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VerificationIo {
    pub files_statd: usize,
    pub files_read: usize,
    pub bytes_hashed: usize,
}

#[cfg(test)]
thread_local! {
    static VERIFICATION_IO: std::cell::Cell<VerificationIo> = const {
        std::cell::Cell::new(VerificationIo { files_statd: 0, files_read: 0, bytes_hashed: 0 })
    };
}

#[cfg(test)]
pub(crate) fn take_verification_io() -> VerificationIo {
    VERIFICATION_IO.with(|io| io.replace(VerificationIo::default()))
}

fn read_verification_source(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let source = std::fs::read(path)?;
    #[cfg(test)]
    VERIFICATION_IO.with(|io| {
        let mut count = io.get();
        count.files_read += 1;
        io.set(count);
    });
    Ok(source)
}

fn verification_key(source: &[u8], language: String, producer: &str) -> String {
    #[cfg(test)]
    VERIFICATION_IO.with(|io| {
        let mut count = io.get();
        count.bytes_hashed += source.len();
        io.set(count);
    });
    crate::blob_store::CallgraphKey::from_bytes(source, language, producer)
        .full_key()
        .to_hex()
}

#[derive(Clone)]
pub(crate) struct VerifiedCallgraphFile {
    size: u64,
    modified: std::time::SystemTime,
    expected_key: String,
}

fn verification_stat(path: &std::path::Path) -> std::io::Result<(u64, std::time::SystemTime)> {
    #[cfg(test)]
    VERIFICATION_IO.with(|io| {
        let mut count = io.get();
        count.files_statd += 1;
        io.set(count);
    });
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.len(), metadata.modified()?))
}

/// Stat-first verification of an immutable manifest. The owner clears `verified`
/// on the same watcher invalidation ticket used by search verification, so an
/// event always rechecks content even if a writer preserved size and mtime.
/// Blocking inspect can reuse its already collected root stats instead of
/// issuing another metadata walk just for this reader.
pub(crate) fn callgraph_paths_match_cached(
    manifest: &super::Manifest,
    root: &std::path::Path,
    paths: &[PathBuf],
    observed: Option<&[(PathBuf, u64, std::time::SystemTime)]>,
    verified: &mut std::collections::BTreeMap<PathBuf, VerifiedCallgraphFile>,
) -> super::Result<bool> {
    let observed = observed.map(|files| {
        files
            .iter()
            .map(|(path, size, modified)| (path.as_path(), (*size, *modified)))
            .collect::<std::collections::BTreeMap<_, _>>()
    });
    for path in paths {
        let Ok(relative) = path.strip_prefix(root) else {
            return Ok(false);
        };
        let key = super::RelPath::from_os_path(relative)?;
        let language = if super::assembly::is_resolution_input(key.as_bytes()) {
            Some("config".to_string())
        } else {
            crate::parser::detect_language(path).map(|lang| format!("{lang:?}").to_lowercase())
        };
        let Some(language) = language else { continue };
        let Some(super::ManifestEntry::Regular { planes, .. }) = manifest.get(&key) else {
            return Ok(false);
        };
        let Some(expected_key) = planes.callgraph.as_ref() else {
            return Ok(false);
        };
        let stats = match observed
            .as_ref()
            .and_then(|files| files.get(path.as_path()))
            .copied()
            .map(Ok)
            .unwrap_or_else(|| verification_stat(path))
        {
            Ok(stats) => stats,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if verified.get(path).is_some_and(|previous| {
            previous.size == stats.0
                && previous.modified == stats.1
                && previous.expected_key == *expected_key
        }) {
            continue;
        }
        let source = read_verification_source(path)?;
        if verification_key(&source, language, super::callgraph::PRODUCER) != *expected_key {
            verified.remove(path);
            return Ok(false);
        }
        // Do not associate verified bytes with metadata from before an edit.
        if verification_stat(path)? != stats {
            return Ok(false);
        }
        verified.insert(
            path.clone(),
            VerifiedCallgraphFile {
                size: stats.0,
                modified: stats.1,
                expected_key: expected_key.clone(),
            },
        );
    }
    Ok(true)
}

/// Why a view generation published with the call graph off is not served.
pub(crate) const CALLGRAPH_DISABLED: &str = "call graph is disabled (indexes.callgraph=false): this view generation was published without call graph data";

/// Open exactly the selected generation. The caller decides whether its snapshot
/// is acceptable; opening a reader must never replace a pinned generation with a
/// newer pointer or silently fall back to the mutable legacy store.
///
/// A generation published with the call graph off has an empty derived
/// database; serving it would answer "no callers" and "no dead code" for a
/// graph that was never built. Published v1 readers refuse it here, as unavailable
/// with [`CALLGRAPH_DISABLED`]; the checkout plane applies the same manifest guard
/// before opening a pinned v2 reader.
/// A manifest that cannot be read is refused too, rather than assumed to
/// carry a call graph.
pub(crate) fn open_published_callgraph(
    project_root: PathBuf,
    family: String,
    view_dir: PathBuf,
    generation: &str,
    pin: Option<Arc<QueryPin>>,
) -> Result<ReadonlyCallGraphStore> {
    if !generation_has_callgraph(&view_dir, generation)? {
        return Err(crate::callgraph_store::CallGraphStoreError::Unavailable(
            CALLGRAPH_DISABLED.to_string(),
        ));
    }
    ReadonlyCallGraphStore::open_manifest_view(project_root, family, view_dir, generation, pin)
}

/// True when the checkout's current view generation (the v1 view under
/// `<storage>/views/<scope>`) exists and was published without call graph
/// data. Read-only: nothing is created when the view is absent.
pub(crate) fn current_generation_lacks_callgraph(
    storage: &std::path::Path,
    root: &std::path::Path,
) -> bool {
    let view_dir = storage
        .join("views")
        .join(crate::path_identity::project_scope_key(root));
    let Some(store) = super::ViewStore::existing_dir(view_dir.clone()) else {
        return false;
    };
    let Ok(Some(generation)) = store.current_generation_read_only() else {
        return false;
    };
    matches!(generation_has_callgraph(&view_dir, &generation), Ok(false))
}

/// Whether `generation` was published with call graph data. Generations are
/// immutable, so the answer is cached per view directory and generation; the
/// manifest is read once, not on every call graph query.
fn generation_has_callgraph(view_dir: &std::path::Path, generation: &str) -> Result<bool> {
    type Cache = std::collections::HashMap<(PathBuf, String), bool>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Cache>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = (view_dir.to_path_buf(), generation.to_owned());
    if let Some(known) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return Ok(*known);
    }
    let unavailable = |reason: String| {
        crate::callgraph_store::CallGraphStoreError::Unavailable(format!(
            "view generation {generation} manifest unreadable: {reason}"
        ))
    };
    let store = super::ViewStore::existing_dir(view_dir.to_path_buf())
        .ok_or_else(|| unavailable("view pointer missing".into()))?;
    let manifest = store
        .load_manifest(generation)
        .map_err(|error| unavailable(error.to_string()))?;
    let has = !super::assembly::manifest_lacks_callgraph(&manifest);
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.len() >= 1024 {
        cache.clear();
    }
    cache.insert(key, has);
    Ok(has)
}

/// Watcher edits do not change HEAD. Refuse to project an older published plane
/// when any requested tracked source still has a different content key.
///
/// Checkout assembly keys every published callgraph plane with the ruled
/// producer, so the current source must be keyed the same way. Keying it with
/// the legacy producer would never match and would leave every published
/// generation looking stale to inspect.
pub(crate) fn callgraph_paths_match(
    manifest: &super::Manifest,
    root: &std::path::Path,
    paths: &[PathBuf],
) -> super::Result<bool> {
    callgraph_paths_match_with_producer(manifest, root, paths, super::callgraph::PRODUCER)
}

/// Content verification for an opted-in ruled callgraph generation. Legacy
/// payload keys are intentionally incompatible with its dispatch-hint producer.
pub fn callgraph_paths_match_v2(
    manifest: &super::manifest_v2::ManifestV2,
    root: &std::path::Path,
    paths: &[PathBuf],
) -> super::Result<bool> {
    let projected = super::callgraph::project_manifest(manifest)
        .map_err(|error| super::ViewError::InvalidManifest(error.to_string()))?;
    callgraph_paths_match_with_producer(&projected, root, paths, super::callgraph::PRODUCER)
}

fn callgraph_paths_match_with_producer(
    manifest: &super::Manifest,
    root: &std::path::Path,
    paths: &[PathBuf],
    producer: &str,
) -> super::Result<bool> {
    for path in paths {
        let Ok(relative) = path.strip_prefix(root) else {
            return Ok(false);
        };
        let key = super::RelPath::from_os_path(relative)?;
        let language = if super::assembly::is_resolution_input(key.as_bytes()) {
            Some("config".to_string())
        } else {
            crate::parser::detect_language(path)
                .map(|language| format!("{language:?}").to_lowercase())
        };
        let Some(language) = language else { continue };
        let entry = manifest.get(&key);
        let source = match read_verification_source(path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if entry.is_some() {
                    return Ok(false);
                }
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        // Untracked paths are not members of the view; their contributions can
        // still be scanned, but they cannot change this published graph.
        let Some(entry) = entry else { continue };
        if let super::ManifestEntry::Regular { planes, .. } = entry {
            let current = verification_key(&source, language, producer);
            if planes.callgraph.as_deref() != Some(current.as_str()) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Opens only the generation protected by the reader's verified marker.
/// No pointer, manifest or plane artifact is created or repaired on this path.
pub fn open_foreign_generation(
    reader: &super::registry::ReaderRegistration,
    scope: &str,
    producers: &super::manifest_v2::Producers,
) -> super::Result<Option<std::sync::Arc<super::snapshot::OpenGeneration>>> {
    let protected = reader.protect_current(scope).map_err(|error| {
        super::ViewError::InvalidManifest(format!("foreign view {scope} unavailable: {error}"))
    })?;
    let Some(protected) = protected else {
        return Ok(None);
    };
    let store =
        super::ViewStore::existing_dir(protected.view_dir().to_path_buf()).ok_or_else(|| {
            super::ViewError::InvalidManifest(format!(
                "foreign view {scope} unavailable: pointer missing"
            ))
        })?;
    let manifest = store.load_manifest_v2(protected.generation())?;
    manifest.ensure_producers(producers)?;
    Ok(Some(std::sync::Arc::new(
        super::snapshot::OpenGeneration::new(
            protected.generation().to_owned(),
            manifest,
            Some(super::snapshot::Residency::Protected(protected)),
        ),
    )))
}

#[cfg(test)]
mod producer_tests {
    use super::*;
    #[test]
    fn ruled_callgraph_content_match_accepts_unchanged_and_rejects_edit() {
        let root = tempfile::tempdir().unwrap();
        let absolute = root.path().join("file.rs");
        let bytes = b"fn target() {}";
        std::fs::write(&absolute, bytes).unwrap();
        let mut entry = super::super::snapshot::LiveEntry::new(
            super::super::snapshot::DiskState::of_bytes(bytes),
            0,
        );
        let attachment = super::super::callgraph::attach(&mut entry, bytes, "rust").unwrap();
        let mut manifest =
            super::super::manifest_v2::ManifestV2::new(super::super::manifest_v2::ManifestHeader {
                producers: super::super::manifest_v2::Producers {
                    trigram: "test".into(),
                    semantic: None,
                    callgraph: super::super::callgraph::PRODUCER.into(),
                },
                head_tree: None,
                ignore_fingerprint: None,
                segment: None,
            });
        manifest
            .insert(
                super::super::RelPath::new(b"file.rs".to_vec()).unwrap(),
                super::super::manifest_v2::EntryV2::regular(
                    crate::blob_store::v2::ContentHash::of(bytes),
                    bytes.len() as u64,
                    super::super::manifest_v2::EntryPlanes {
                        callgraph: Some(super::super::readiness::PlaneState::ready(
                            &attachment.key,
                        )),
                        ..Default::default()
                    },
                ),
            )
            .unwrap();
        assert!(callgraph_paths_match_v2(&manifest, root.path(), &[absolute.clone()]).unwrap());
        std::fs::write(&absolute, b"fn changed() {}").unwrap();
        assert!(!callgraph_paths_match_v2(&manifest, root.path(), &[absolute]).unwrap());
    }

    #[test]
    #[ignore = "manual source-verification IO measurement on the worker checkout"]
    fn inspect_repository_source_verification_io_profile() {
        let root = std::fs::canonicalize(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap(),
        )
        .unwrap();
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        let mut entries = Vec::new();
        let mut paths = Vec::new();
        let mut stats = Vec::new();
        for path in files {
            let relative =
                super::super::RelPath::from_os_path(path.strip_prefix(&root).unwrap()).unwrap();
            let language = if super::super::assembly::is_resolution_input(relative.as_bytes()) {
                Some("config".to_string())
            } else {
                crate::parser::detect_language(&path).map(|lang| format!("{lang:?}").to_lowercase())
            };
            let Some(language) = language else { continue };
            let bytes = std::fs::read(&path).unwrap();
            let metadata = std::fs::metadata(&path).unwrap();
            let key = crate::blob_store::CallgraphKey::from_bytes(
                &bytes,
                language,
                super::super::callgraph::PRODUCER,
            )
            .full_key()
            .to_hex();
            entries.push((
                relative,
                super::super::ManifestEntry::Regular {
                    mode: 0o100644,
                    planes: super::super::RegularPlanes {
                        semantic: None,
                        callgraph: Some(key),
                    },
                    resolution_input: false,
                },
            ));
            stats.push((path.clone(), metadata.len(), metadata.modified().unwrap()));
            paths.push(path);
        }
        let manifest = super::super::Manifest::new(entries).unwrap();
        // This is the source-verification part of the reader, not a fabricated
        // cold callgraph benchmark. Generation assembly is deliberately excluded.
        take_verification_io();
        for _ in 0..2 {
            assert!(callgraph_paths_match(&manifest, &root, &paths).unwrap());
        }
        let before = take_verification_io();
        let mut verified = std::collections::BTreeMap::new();
        assert!(callgraph_paths_match_cached(
            &manifest,
            &root,
            &paths,
            Some(&stats),
            &mut verified
        )
        .unwrap());
        let cold = take_verification_io();
        assert!(callgraph_paths_match_cached(
            &manifest,
            &root,
            &paths,
            Some(&stats),
            &mut verified
        )
        .unwrap());
        let warm = take_verification_io();
        eprintln!("inspect_repo_source_verification root={} source_files={} previous_per_call={before:?} revised_cold={cold:?} revised_warm={warm:?}", root.display(), paths.len());
        assert_eq!(warm, VerificationIo::default());
        assert_eq!(before.files_read, paths.len() * 2);
        assert_eq!(cold.files_read, paths.len());
    }
}
