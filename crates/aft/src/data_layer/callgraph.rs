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
//! 4. **Method calls**: every file's method-call sites against every file's
//!    method-call facts, including the import targets other files' pass 2
//!    found and the type targets their pass 3 found. Each file records the
//!    dependency keys its resolutions read.
//!
//! The resolver's memos and recorders are single-threaded, so each thread
//! has its own; they are caches and recorders, never inputs, so results do
//! not depend on which thread resolves which file.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use aft_store::checkout::{CheckoutState, PathKind};
use rayon::prelude::*;

use crate::callgraph_store::facts::{BlobKey, DirEntry, EntryKind, ProjectFacts};
use crate::callgraph_store::join::dispatch::{
    associated_reference, dependency, MemoizedResolver, Resolution, Resolver, SiteHint,
};
use crate::callgraph_store::join::{
    bind_file, dispatch_type_targets_for, finish_binding, resolve_file, BlobRef, BlobRefKind,
    DerivedRow, DispatchTypeTargets, ManifestJoinError, ParseBlob, ViewBindingDependencies,
    ViewBindingFacts,
};
use crate::callgraph_store::{DbFileIndex, ProjectIndex, WorkspaceCratePrefixCache};
use crate::views::materialization::dispatch::FileFacts;

use super::graph_rows::references_by_key;

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

/// One method-call site's resolution, as pass 4 keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SiteResolution {
    pub(crate) site: SiteHint,
    pub(crate) resolution: Resolution,
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
    /// Pass 4: the method-call sites kept, in site order.
    pub(crate) sites: Vec<SiteResolution>,
    /// Pass 4: the dependency keys its site resolutions read.
    pub(crate) site_dependencies: BTreeSet<String>,
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
                        ..FileResolution::default()
                    },
                ))
            },
        )
        .collect::<Result<BTreeMap<_, _>, ManifestJoinError>>()?;
    resolve_method_calls(inputs, resolved)
}

/// What pass 4 reads about one file beyond its own facts: which of its
/// references Rust spells as an associated or `self.` call, and which of
/// its references already have an ordinary edge from pass 2.
#[derive(Default)]
struct OrdinaryCalls {
    /// `view:<path>:<key>` of call and construction references spelled as an
    /// associated function or a `self.` call.
    syntax: HashSet<String>,
    /// Reference keys of associated-function calls.
    associated: Vec<u32>,
    /// `view:<path>:<key>` of references pass 2 linked from a caller symbol
    /// to a target symbol.
    edges: HashSet<String>,
}

fn resolve_method_calls(
    inputs: &CheckoutInputs,
    mut resolved: BTreeMap<String, FileResolution>,
) -> Result<BTreeMap<String, FileResolution>, ManifestJoinError> {
    let mut facts = BTreeMap::new();
    let mut ordinary = HashMap::new();
    for (path, (_, parse)) in &inputs.parses {
        let file = &resolved[path];
        let references = references_by_key(parse);
        let mut file_facts = FileFacts::new(parse);
        file_facts.import_targets = import_targets(&file.rows, &references);
        if let Some(dispatch) = &file.dispatch {
            file_facts.type_targets = dispatch.targets.clone();
        }
        ordinary.insert(
            path.clone(),
            ordinary_calls(path, parse, &file.rows, &references),
        );
        facts.insert(path.clone(), file_facts);
    }
    let resolver_parses = facts
        .iter()
        .map(|(path, facts)| (path.clone(), facts.resolver_parse()))
        .collect::<BTreeMap<_, _>>();
    let import_targets = facts
        .iter()
        .flat_map(|(path, facts)| {
            facts
                .import_targets
                .iter()
                .map(move |(module, target)| ((path.clone(), module.clone()), target.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let type_targets = facts
        .iter()
        .flat_map(|(path, facts)| {
            facts
                .type_targets
                .iter()
                .map(move |(name, target)| ((path.clone(), name.clone()), target.clone()))
        })
        .collect::<BTreeMap<_, _>>();

    let paths = inputs.parses.keys().collect::<Vec<_>>();
    let chunk = paths
        .len()
        .div_ceil(rayon::current_num_threads() * 4)
        .max(1);
    let results = paths
        .par_chunks(chunk)
        .map(|chunk| {
            // The resolver and its memo are single-threaded caches over
            // shared inputs: one per chunk of files.
            let mut resolver = Resolver::new(
                resolver_parses
                    .iter()
                    .map(|(path, parse)| (path.clone(), parse))
                    .collect(),
            );
            resolver.import_targets = import_targets.clone();
            resolver.type_targets = type_targets.clone();
            for (path, calls) in &ordinary {
                for key in &calls.associated {
                    resolver.note_associated_site(path, *key);
                }
            }
            let memoized = MemoizedResolver::new(&resolver);
            resolver.take_dependencies();
            chunk
                .iter()
                .map(|path| {
                    let (_, parse) = &inputs.parses[*path];
                    let calls = &ordinary[*path];
                    let mut sites = Vec::new();
                    for site in parse.bound_dispatch_sites() {
                        let ref_id = format!("view:{path}:{}", site.ordinal);
                        let resolution = memoized.resolve(path, &site);
                        // Keep proven self-call and scoped module-function
                        // bindings when dispatch facts do not identify a
                        // project receiver type.
                        if parse.language == "rust"
                            && resolution.external > 0
                            && resolution.targets.is_empty()
                            && calls.edges.contains(&ref_id)
                            && calls.syntax.contains(&ref_id)
                        {
                            continue;
                        }
                        sites.push(SiteResolution {
                            site,
                            resolution: (*resolution).clone(),
                        });
                    }
                    // Every file depends on its own facts, which
                    // `Resolver::resolve` reads without recording.
                    let mut dependencies = resolver.take_dependencies();
                    dependencies.insert(dependency::file(path));
                    ((*path).clone(), sites, dependencies)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    for (path, sites, dependencies) in results.into_iter().flatten() {
        let file = resolved
            .get_mut(&path)
            .expect("every parsed file was resolved");
        file.sites = sites;
        file.site_dependencies = dependencies;
    }
    Ok(resolved)
}

/// The target file of each module a file imports, from its pass-2 rows.
/// For a module imported from two targets, the greater path wins, as when
/// they were read back sorted.
fn import_targets(
    rows: &[DerivedRow],
    references: &HashMap<u32, &BlobRef>,
) -> BTreeMap<String, String> {
    let mut pairs = rows
        .iter()
        .filter(|row| row.kind == BlobRefKind::Import)
        .filter_map(|row| {
            let module = references.get(&row.ref_ordinal)?.module_path.clone()?;
            let target = String::from_utf8(row.target_path.clone()?).ok()?;
            Some((module, target))
        })
        .collect::<Vec<_>>();
    pairs.sort();
    pairs.into_iter().collect()
}

fn ordinary_calls(
    path: &str,
    parse: &ParseBlob,
    rows: &[DerivedRow],
    references: &HashMap<u32, &BlobRef>,
) -> OrdinaryCalls {
    let mut calls = OrdinaryCalls::default();
    let mut symbols = HashSet::new();
    for symbol in &parse.symbols {
        symbols.insert(symbol.scoped_name.as_str());
        symbols.insert(symbol.name.as_str());
    }
    for row in rows {
        let Some(reference) = references.get(&row.ref_ordinal) else {
            continue;
        };
        let ref_id = format!("view:{path}:{}", row.ref_ordinal);
        let full = reference.full_ref.as_deref().filter(|_| {
            parse.language == "rust"
                && matches!(row.kind, BlobRefKind::Call | BlobRefKind::Construction)
        });
        if let Some(full) = full {
            let associated = associated_reference(full);
            if associated {
                calls.associated.push(row.ref_ordinal);
            }
            if associated || full.starts_with("self.") {
                calls.syntax.insert(ref_id.clone());
            }
        }
        if matches!(
            row.kind,
            BlobRefKind::Call | BlobRefKind::Construction | BlobRefKind::CallbackRef
        ) && reference
            .caller_symbol
            .as_deref()
            .is_some_and(|symbol| symbols.contains(symbol))
            && row.target_path.is_some()
            && row.target_symbol.is_some()
        {
            calls.edges.insert(ref_id);
        }
    }
    calls
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
