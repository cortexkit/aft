//! Reuse live workspace answers only while their filesystem inputs still match.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use crate::callgraph_store::facts::{byte_path, DirEntry, FactPaths, ProjectFacts};

const MAX_CACHED_ROOTS: usize = 64;
const MAX_INPUTS_PER_ROOT: usize = 4096;

#[derive(PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    len: u64,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

fn stamp(path: &Path) -> io::Result<Option<Stamp>> {
    let metadata = match path.metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Some(Stamp {
        modified: metadata.modified()?,
        len: metadata.len(),
        #[cfg(unix)]
        identity: (
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ),
    }))
}

type Stamps = HashMap<PathBuf, Option<Stamp>>;

fn fresh(paths: &Stamps) -> bool {
    paths
        .iter()
        .all(|(path, previous)| stamp(path).is_ok_and(|current| current == *previous))
}

struct Entry<T> {
    answer: Arc<T>,
    paths: Stamps,
}

pub(super) struct LiveCache<T>(HashMap<PathBuf, Arc<Entry<T>>>);

impl<T> Default for LiveCache<T> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<T> LiveCache<T> {
    pub(super) fn get_or_build(
        cache: &RwLock<Self>,
        root: &Path,
        build: impl FnOnce(&Inputs) -> T,
    ) -> Arc<T> {
        let prior = cache
            .read()
            .ok()
            .and_then(|cache| cache.0.get(root).cloned());
        if let Some(prior) = prior {
            if fresh(&prior.paths) {
                return Arc::clone(&prior.answer);
            }
        }
        let inputs = Inputs(RefCell::new(Some(HashMap::new())));
        let answer = Arc::new(build(&inputs));
        if let Some(paths) = inputs.0.into_inner().filter(fresh) {
            if let Ok(mut cache) = cache.write() {
                if cache.0.len() >= MAX_CACHED_ROOTS && !cache.0.contains_key(root) {
                    if let Some(victim) = cache.0.keys().next().cloned() {
                        cache.0.remove(&victim);
                    }
                }
                cache.0.insert(
                    root.to_path_buf(),
                    Arc::new(Entry {
                        answer: Arc::clone(&answer),
                        paths,
                    }),
                );
            }
        }
        answer
    }

    pub(super) fn clear_under(&mut self, root: &Path) {
        self.0.retain(|key, _| !key.starts_with(root));
    }
}

pub(super) struct Inputs(RefCell<Option<Stamps>>);

impl Inputs {
    pub(super) fn track(&self, path: &Path) {
        let mut state = self.0.borrow_mut();
        let Some(paths) = state.as_mut() else { return };
        if paths.contains_key(path) {
            return;
        }
        // Large, unreadable or unstable walks still resolve normally but cannot
        // produce a reusable answer with complete change evidence.
        if paths.len() >= MAX_INPUTS_PER_ROOT {
            *state = None;
        } else if let Ok(current) = stamp(path) {
            paths.insert(path.to_path_buf(), current);
        } else {
            *state = None;
        }
    }

    pub(super) fn observe<'a>(&'a self, facts: &'a FactPaths<'a>) -> ObservedFacts<'a> {
        ObservedFacts {
            inputs: self,
            root: facts.root,
            facts: facts.facts,
        }
    }
}

pub(super) struct ObservedFacts<'a> {
    inputs: &'a Inputs,
    root: &'a Path,
    facts: &'a dyn ProjectFacts,
}

impl ObservedFacts<'_> {
    fn track(&self, rel: &[u8]) {
        self.inputs.track(&self.root.join(byte_path(rel)));
    }
}

impl ProjectFacts for ObservedFacts<'_> {
    fn is_file(&self, rel: &[u8]) -> bool {
        self.track(rel);
        self.facts.is_file(rel)
    }

    fn is_dir(&self, rel: &[u8]) -> bool {
        self.track(rel);
        self.facts.is_dir(rel)
    }

    fn config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        self.track(rel);
        self.facts.config_bytes(rel)
    }

    fn attributed_config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        self.track(rel);
        self.facts.attributed_config_bytes(rel)
    }

    fn symlink_target(&self, rel: &[u8]) -> Option<&[u8]> {
        self.track(rel);
        self.facts.symlink_target(rel)
    }

    fn canonical(&self, rel: &[u8]) -> Option<Vec<u8>> {
        self.track(rel);
        self.facts.canonical(rel)
    }

    fn canonical_probe(&self, rel: &[u8]) -> Option<Vec<u8>> {
        self.track(rel);
        self.facts.canonical_probe(rel)
    }

    fn canonical_path(&self, root: &Path, rel: &[u8]) -> Option<PathBuf> {
        self.inputs.track(&root.join(byte_path(rel)));
        self.facts.canonical_path(root, rel)
    }

    fn list_dir(&self, rel: &[u8]) -> Vec<DirEntry> {
        // Directory stamps also detect new member directories and manifests for
        // which there was no file stamp when the answer was built.
        self.track(rel);
        self.facts.list_dir(rel)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn review_opus7_manifest_stamp_tracks_same_mtime_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let manifest = root.join("package.json");
        fs::write(&manifest, r#"{"name":"one"}"#).unwrap();
        let original = manifest.metadata().unwrap();
        let cache = RwLock::default();
        let lookup = || {
            LiveCache::get_or_build(&cache, root, |inputs| {
                inputs.track(&manifest);
                fs::read_to_string(&manifest).unwrap()
            })
        };
        let before = lookup();
        assert!(Arc::ptr_eq(&before, &lookup()));

        let replacement = root.join("package.next");
        fs::write(&replacement, r#"{"name":"two"}"#).unwrap();
        fs::File::open(&replacement)
            .unwrap()
            .set_modified(original.modified().unwrap())
            .unwrap();
        fs::rename(&replacement, &manifest).unwrap();
        let current = manifest.metadata().unwrap();
        assert_eq!(original.len(), current.len());
        assert_eq!(original.modified().unwrap(), current.modified().unwrap());
        assert_eq!(lookup().as_str(), r#"{"name":"two"}"#);
    }
}
