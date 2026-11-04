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
#[derive(Clone, Debug)]
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

/// The bound surfaces of every parsed file, shared read-only by resolution.
pub(crate) type Surfaces = Arc<HashMap<String, DbFileIndex>>;

/// Resolves every parsed file of a checkout on `threads` threads.
///
/// # Errors
///
/// When the resolver fails on a file.
pub(crate) fn resolve_checkout(
    inputs: &CheckoutInputs,
    threads: usize,
) -> Result<BTreeMap<String, FileResolution>, ManifestJoinError> {
    pool(threads)?.install(|| {
        let paths = inputs.parses.keys().collect::<Vec<_>>();
        let (surfaces, prebound) = bind_all(inputs, &paths)?;
        let surfaces: Surfaces = Arc::new(surfaces);
        let mut resolved = resolve_files(inputs, &surfaces, &prebound, &paths)?;
        let calls = MethodCallInputs::build(inputs, &resolved);
        for (path, sites, dependencies) in calls.resolve_sites(inputs, &paths) {
            let file = resolved
                .get_mut(&path)
                .expect("every parsed file was resolved");
            file.sites = sites;
            file.site_dependencies = dependencies;
        }
        Ok(resolved)
    })
}

pub(crate) fn pool(threads: usize) -> Result<rayon::ThreadPool, ManifestJoinError> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .thread_name(|index| format!("aft-resolve-{index}"))
        .build()
        .map_err(|error| ManifestJoinError::Parse(error.to_string()))
}

fn thread_facts(inputs: &CheckoutInputs) -> ViewBindingFacts<'_> {
    ViewBindingFacts::new(Rc::new(CheckoutFacts { inputs }))
}

/// Pass 1 for `paths`, in parallel: each file's bound surface, and its
/// binding before resolution.
///
/// # Errors
///
/// When binding fails on a file.
pub(crate) fn bind_all(
    inputs: &CheckoutInputs,
    paths: &[&String],
) -> Result<
    (
        HashMap<String, DbFileIndex>,
        HashMap<String, ViewBindingDependencies>,
    ),
    ManifestJoinError,
> {
    let bound = paths
        .par_iter()
        .map_init(
            || thread_facts(inputs),
            |facts, path| {
                let (_, parse) = &inputs.parses[path.as_str()];
                bind_file(path, parse, facts, None).map(|bound| ((*path).clone(), bound))
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    let mut surfaces = HashMap::with_capacity(bound.len());
    let mut prebound = HashMap::with_capacity(bound.len());
    for (path, (surface, binding)) in bound {
        surfaces.insert(path.clone(), surface);
        prebound.insert(path, binding);
    }
    Ok((surfaces, prebound))
}

/// Passes 2 and 3 for `paths`, in parallel, against every file's bound
/// surface. Method-call sites are left empty, for [`MethodCallInputs`].
///
/// # Errors
///
/// When resolution fails on a file.
pub(crate) fn resolve_files(
    inputs: &CheckoutInputs,
    surfaces: &Surfaces,
    prebound: &HashMap<String, ViewBindingDependencies>,
    paths: &[&String],
) -> Result<BTreeMap<String, FileResolution>, ManifestJoinError> {
    paths
        .par_iter()
        .map_init(
            || {
                let facts = Rc::new(thread_facts(inputs));
                let index = resolver_index(surfaces, &facts);
                (facts, index)
            },
            |(facts, index), path| {
                let (key, parse) = &inputs.parses[path.as_str()];
                let mut binding = prebound
                    .get(path.as_str())
                    .cloned()
                    .expect("every resolved file was bound");
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
                let dispatch = dispatch_targets(path, parse, index, facts)?;
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
        .collect()
}

/// A resolver index over `surfaces`, recording into `facts`.
pub(crate) fn resolver_index<'a>(
    surfaces: &Surfaces,
    facts: &Rc<ViewBindingFacts<'a>>,
) -> ProjectIndex<'a> {
    ProjectIndex::from_shared(
        std::path::Path::new("/"),
        Arc::clone(surfaces),
        HashMap::new(),
        WorkspaceCratePrefixCache::default(),
        facts.clone(),
    )
}

/// Pass 3 for one file: for Rust, TypeScript and JavaScript, the targets of
/// its type names.
pub(crate) fn dispatch_targets(
    path: &str,
    parse: &ParseBlob,
    index: &ProjectIndex<'_>,
    facts: &ViewBindingFacts<'_>,
) -> Result<Option<DispatchTypeTargets>, ManifestJoinError> {
    matches!(
        parse.language.as_str(),
        "rust" | "typescript" | "tsx" | "javascript"
    )
    .then(|| dispatch_type_targets_for(path, parse, index, facts))
    .transpose()
}

/// What pass 4 reads about one file beyond its own facts: which of its
/// references Rust spells as an associated or `self.` call, and which of
/// its references already have an ordinary edge from pass 2.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OrdinaryCalls {
    /// `view:<path>:<key>` of call and construction references spelled as an
    /// associated function or a `self.` call.
    syntax: HashSet<String>,
    /// Reference keys of associated-function calls.
    associated: Vec<u32>,
    /// `view:<path>:<key>` of references pass 2 linked from a caller symbol
    /// to a target symbol.
    edges: HashSet<String>,
}

/// Every file's method-call facts, as pass 4 reads them. Kept by a live
/// checkout and updated file by file.
#[derive(Debug, Default)]
pub(crate) struct MethodCallInputs {
    pub(crate) facts: BTreeMap<String, FileFacts>,
    resolver_parses: BTreeMap<String, ParseBlob>,
    ordinary: HashMap<String, OrdinaryCalls>,
    import_targets: BTreeMap<(String, String), String>,
    type_targets: BTreeMap<(String, String), (String, String)>,
}

impl MethodCallInputs {
    pub(crate) fn build(
        inputs: &CheckoutInputs,
        resolved: &BTreeMap<String, FileResolution>,
    ) -> Self {
        let mut calls = Self::default();
        for path in inputs.parses.keys() {
            calls.update(inputs, path, resolved.get(path));
        }
        calls
    }

    /// The facts of one file as `resolution` leaves them, or none when the
    /// file is gone. Returns the dependency keys whose readers the change
    /// can affect, and whether the file's own pass-4 inputs changed.
    pub(crate) fn update(
        &mut self,
        inputs: &CheckoutInputs,
        path: &str,
        resolution: Option<&FileResolution>,
    ) -> (BTreeSet<String>, bool) {
        let new = resolution
            .zip(inputs.parses.get(path))
            .map(|(file, (_, parse))| {
                let references = references_by_key(parse);
                let mut facts = FileFacts::new(parse);
                facts.import_targets = import_targets(&file.rows, &references);
                if let Some(dispatch) = &file.dispatch {
                    facts.type_targets = dispatch.targets.clone();
                }
                (facts, ordinary_calls(path, parse, &file.rows, &references))
            });
        let old_facts = self.facts.get(path);
        let mut keys = BTreeSet::new();
        FileFacts::changed_keys(
            path,
            old_facts,
            new.as_ref().map(|(facts, _)| facts),
            &mut keys,
        );
        let own_changed = keys.contains(&dependency::file(path))
            || self.ordinary.get(path) != new.as_ref().map(|(_, calls)| calls);
        if let Some(old) = self.facts.remove(path) {
            for module in old.import_targets.keys() {
                self.import_targets
                    .remove(&(path.to_string(), module.clone()));
            }
            for name in old.type_targets.keys() {
                self.type_targets.remove(&(path.to_string(), name.clone()));
            }
        }
        self.resolver_parses.remove(path);
        self.ordinary.remove(path);
        if let Some((facts, calls)) = new {
            for (module, target) in &facts.import_targets {
                self.import_targets
                    .insert((path.to_string(), module.clone()), target.clone());
            }
            for (name, target) in &facts.type_targets {
                self.type_targets
                    .insert((path.to_string(), name.clone()), target.clone());
            }
            self.resolver_parses
                .insert(path.to_string(), facts.resolver_parse());
            self.facts.insert(path.to_string(), facts);
            self.ordinary.insert(path.to_string(), calls);
        }
        (keys, own_changed)
    }

    /// Pass 4 for `paths`, in parallel: each file's kept method-call sites
    /// and the dependency keys their resolutions read.
    pub(crate) fn resolve_sites(
        &self,
        inputs: &CheckoutInputs,
        paths: &[&String],
    ) -> Vec<(String, Vec<SiteResolution>, BTreeSet<String>)> {
        let chunk = paths
            .len()
            .div_ceil(rayon::current_num_threads() * 4)
            .max(1);
        paths
            .par_chunks(chunk)
            .map(|chunk| {
                // The resolver and its memo are single-threaded caches over
                // shared inputs: one per chunk of files.
                let mut resolver = Resolver::new(
                    self.resolver_parses
                        .iter()
                        .map(|(path, parse)| (path.clone(), parse))
                        .collect(),
                );
                resolver.import_targets = self.import_targets.clone();
                resolver.type_targets = self.type_targets.clone();
                for (path, calls) in &self.ordinary {
                    for key in &calls.associated {
                        resolver.note_associated_site(path, *key);
                    }
                }
                let memoized = MemoizedResolver::new(&resolver);
                resolver.take_dependencies();
                chunk
                    .iter()
                    .map(|path| {
                        let (_, parse) = &inputs.parses[path.as_str()];
                        let calls = &self.ordinary[path.as_str()];
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
            .flatten()
            .collect()
    }
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
pub(crate) struct CheckoutFacts<'a> {
    pub(crate) inputs: &'a CheckoutInputs,
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
