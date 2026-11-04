//! Call-graph resolution over a checkout's content records, one file at a
//! time and in parallel.
//!
//! Resolution has three passes, each a function of the resolver's (see
//! `callgraph_store::join`):
//!
//! 1. **Bind** every parsed file against the checkout's paths: its module
//!    targets and the rest of its resolver surface. Each file is bound on its
//!    own, so the pass runs on every thread at once.
//! 2. **Resolve** every file's references against all bound surfaces, which
//!    the threads share read-only, recording each surface query with its
//!    answer and each path or configuration field consulted.
//! 3. **Dispatch targets**: for Rust, TypeScript and JavaScript files, the
//!    files their type names resolve to, for method-call resolution.
//!
//! The resolver's memos and recorders are single-threaded, so each thread
//! has its own; they are caches and recorders, never inputs, so results do
//! not depend on which thread resolves which file.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use aft_store::checkout::{CheckoutState, PathKind};
use rayon::prelude::*;

use crate::callgraph_store::facts::{BlobKey, DirEntry, EntryKind, ProjectFacts};
use crate::callgraph_store::join::{
    DerivedRow, DispatchTypeTargets, ManifestJoinError, ParseBlob, ViewBindingDependencies,
    ViewBindingFacts, bind_file, dispatch_type_targets_for, finish_binding, resolve_file,
};
use crate::callgraph_store::{DbFileIndex, ProjectIndex, WorkspaceCratePrefixCache};

/// What resolving a checkout reads.
#[derive(Debug)]
pub(crate) struct CheckoutInputs {
    /// The checkout's paths.
    pub(crate) state: CheckoutState,
    /// Every parsed file's record key and parse output, by path.
    pub(crate) parses: BTreeMap<String, (BlobKey, Arc<ParseBlob>)>,
    /// The raw bytes of every resolver configuration file (`package.json`,
    /// `Cargo.toml`, `go.mod`, `tsconfig*.json`, `.gitignore`), by path.
    pub(crate) configs: HashMap<Vec<u8>, Arc<[u8]>>,
}

/// One file's resolution.
#[derive(Debug, Default)]
pub(crate) struct FileResolution {
    /// One row per reference.
    pub(crate) rows: Vec<DerivedRow>,
    /// What binding and resolution consulted, and the surface queries
    /// resolution asked with their answers.
    pub(crate) binding: ViewBindingDependencies,
    /// For Rust, TypeScript and JavaScript files, the targets of their type
    /// names.
    pub(crate) dispatch: Option<DispatchTypeTargets>,
}

/// Resolves every parsed file of a checkout on `threads` threads.
///
/// # Errors
///
/// When the resolver fails on a file.
pub(crate) fn resolve_checkout(
    inputs: &CheckoutInputs,
    threads: usize,
) -> Result<BTreeMap<String, FileResolution>, ManifestJoinError> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .thread_name(|index| format!("aft-resolve-{index}"))
        .build()
        .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
    pool.install(|| resolve_in_pool(inputs))
}

fn resolve_in_pool(
    inputs: &CheckoutInputs,
) -> Result<BTreeMap<String, FileResolution>, ManifestJoinError> {
    let parses = inputs.parses.iter().collect::<Vec<_>>();
    let facts_for_thread = || ViewBindingFacts::new(Rc::new(CheckoutFacts { inputs }));

    // Pass 1: bind every file.
    let bound = parses
        .par_iter()
        .map_init(facts_for_thread, |facts, (path, (_, parse))| {
            bind_file(path, parse, facts, None).map(|bound| ((*path).clone(), bound))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut surfaces = HashMap::with_capacity(bound.len());
    let mut bindings = HashMap::with_capacity(bound.len());
    for (path, (surface, binding)) in bound {
        surfaces.insert(path.clone(), surface);
        bindings.insert(path, binding);
    }
    let surfaces: Arc<HashMap<String, DbFileIndex>> = Arc::new(surfaces);

    // Passes 2 and 3: resolve every file against the shared surfaces.
    let resolved = parses
        .par_iter()
        .map_init(
            || {
                let facts = Rc::new(facts_for_thread());
                let index = ProjectIndex::from_shared(
                    std::path::Path::new("/"),
                    Arc::clone(&surfaces),
                    HashMap::new(),
                    WorkspaceCratePrefixCache::default(),
                    facts.clone(),
                );
                (facts, index)
            },
            |(facts, index), (path, (key, parse))| {
                let mut binding = bindings
                    .get(path.as_str())
                    .cloned()
                    .expect("every parsed file was bound");
                let mut rows = BTreeSet::new();
                resolve_file(
                    path,
                    key,
                    parse,
                    &mut binding,
                    index,
                    facts,
                    None,
                    &mut rows,
                )?;
                finish_binding(&mut binding);
                let dispatch = matches!(
                    parse.language.as_str(),
                    "rust" | "typescript" | "tsx" | "javascript"
                )
                .then(|| dispatch_type_targets_for(path, parse, index, facts))
                .transpose()?;
                Ok((
                    (*path).clone(),
                    FileResolution {
                        rows: rows.into_iter().collect(),
                        binding,
                        dispatch,
                    },
                ))
            },
        )
        .collect::<Result<BTreeMap<_, _>, ManifestJoinError>>()?;
    Ok(resolved)
}

/// The checkout's paths and configuration, as the resolver asks for them.
/// Answers what the views manifest answered for the same checkout.
#[derive(Debug)]
struct CheckoutFacts<'a> {
    inputs: &'a CheckoutInputs,
}

impl CheckoutFacts<'_> {
    fn exists(&self, rel: &[u8]) -> bool {
        self.inputs.state.get(rel).is_some()
    }
}

impl ProjectFacts for CheckoutFacts<'_> {
    fn is_file(&self, rel: &[u8]) -> bool {
        self.inputs
            .state
            .get(rel)
            .is_some_and(|record| record.kind != PathKind::Symlink)
    }

    fn is_dir(&self, rel: &[u8]) -> bool {
        let state = &self.inputs.state;
        if rel.is_empty() {
            return !state.paths().is_empty();
        }
        !state.directory(rel).is_empty()
    }

    fn config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        let path = self.canonical(rel)?;
        self.inputs.configs.get(&path).cloned()
    }

    fn symlink_target(&self, rel: &[u8]) -> Option<&[u8]> {
        self.inputs
            .state
            .get(rel)
            .filter(|record| record.kind == PathKind::Symlink)
            .map(|record| record.link.as_slice())
    }

    /// The path `rel` names after following symlinks inside the checkout,
    /// or `None` when it leaves the checkout, loops, or names nothing.
    fn canonical(&self, rel: &[u8]) -> Option<Vec<u8>> {
        if rel.starts_with(b"/") {
            return None;
        }
        let mut pending = rel
            .split(|b| *b == b'/')
            .map(<[u8]>::to_vec)
            .collect::<std::collections::VecDeque<_>>();
        let mut components = Vec::<Vec<u8>>::new();
        let mut seen = BTreeSet::new();
        let mut followed = 0;
        while let Some(component) = pending.pop_front() {
            match component.as_slice() {
                b"" | b"." => continue,
                b".." => {
                    components.pop()?;
                    continue;
                }
                _ => components.push(component),
            }
            let path = components.join(&b'/');
            if let Some(target) = self.symlink_target(&path) {
                // Bound expanding cycles such as a -> a/a as well as exact cycles.
                followed += 1;
                if target.starts_with(b"/")
                    || followed > 40
                    || !seen.insert((path, pending.clone()))
                {
                    return None;
                }
                components.pop();
                for component in target.split(|b| *b == b'/').rev() {
                    pending.push_front(component.to_vec());
                }
            } else if !pending.is_empty() && !self.is_dir(&path) {
                return None;
            }
        }
        let path = components.join(&b'/');
        (self.exists(&path) || self.is_dir(&path)).then_some(path)
    }

    fn list_dir(&self, rel: &[u8]) -> Vec<DirEntry> {
        let mut prefix = rel.to_vec();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let state = &self.inputs.state;
        let range = if rel.is_empty() {
            0..state.paths().len()
        } else {
            state.directory(rel)
        };
        let mut entries = BTreeMap::new();
        for record in &state.paths()[range] {
            let Some(tail) = record.path.strip_prefix(prefix.as_slice()) else {
                continue;
            };
            if tail.is_empty() {
                continue;
            }
            if let Some(slash) = tail.iter().position(|b| *b == b'/') {
                entries
                    .entry(tail[..slash].to_vec())
                    .or_insert(EntryKind::Directory);
            } else {
                let kind = if record.kind == PathKind::Symlink {
                    EntryKind::Symlink
                } else {
                    EntryKind::Regular
                };
                entries.insert(tail.to_vec(), kind);
            }
        }
        entries
            .into_iter()
            .map(|(name, kind)| DirEntry { name, kind })
            .collect()
    }
}
