//! A resolved checkout kept up to date edit by edit.
//!
//! After an edit, each pass redoes only what its recorded inputs say can
//! have changed:
//!
//! 1. **Bind** the changed files, and every file whose binding probed a path
//!    that was added or removed, or read a configuration file that changed.
//! 2. **Resolve** again every rebound file whose own content or reference
//!    bindings changed. Every other file whose recorded surface queries
//!    depend on a changed surface (a path, or a domain such as the Rust
//!    module tree) replays those queries against the new surfaces, and is
//!    resolved again only when an answer differs.
//! 3. **Dispatch targets**: likewise, replaying each trace's queries.
//! 4. **Method calls**: every file whose method-call facts changed names the
//!    dependency keys its readers recorded; those readers, and every file
//!    whose own facts or parse changed, resolve their sites again.
//!
//! Nothing feeds back: passes 2 and 3 read only pass-1 surfaces, and pass 4
//! reads the outputs of passes 2 and 3, so one round reaches the result a
//! cold build gives.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use crate::callgraph_store::join::{
    ManifestJoinError, ViewBindingDependencies, ViewBindingFacts, VIEW_CONFIG_MEMBERSHIP_DOMAIN,
    VIEW_GO_PACKAGE_DOMAIN, VIEW_RUST_MODULE_DOMAIN,
};

use super::callgraph::{
    bind_all, dispatch_targets, resolve_files, resolver_index, CheckoutFacts, CheckoutInputs,
    FileResolution, MethodCallInputs, Surfaces,
};

/// What one update did, for measurement.
#[derive(Clone, Debug, Default)]
pub struct UpdateStats {
    pub changed_paths: usize,
    pub rebound: usize,
    pub surfaces_changed: usize,
    pub replayed: usize,
    pub kept_after_replay: usize,
    pub resolved: usize,
    pub dispatch_replayed: usize,
    pub dispatch_recomputed: usize,
    pub facts_changed: usize,
    pub sites_resolved: usize,
    pub bind_ms: f64,
    pub resolve_ms: f64,
    pub dispatch_ms: f64,
    pub sites_ms: f64,
    pub total_ms: f64,
}

/// A checkout's resolution, with what each pass recorded.
pub struct LiveCheckout {
    pub(crate) inputs: CheckoutInputs,
    surfaces: Surfaces,
    /// Each file's binding as pass 1 left it.
    prebound: HashMap<String, ViewBindingDependencies>,
    pub(crate) resolved: BTreeMap<String, FileResolution>,
    calls: MethodCallInputs,
    pool: rayon::ThreadPool,
}

impl LiveCheckout {
    /// Resolves every file of `inputs` cold.
    ///
    /// # Errors
    ///
    /// When resolution fails on a file.
    pub(crate) fn cold(inputs: CheckoutInputs, threads: usize) -> Result<Self, ManifestJoinError> {
        let pool = super::callgraph::pool(threads)?;
        let (surfaces, prebound, resolved, calls) = pool.install(|| {
            let paths = inputs.parses.keys().collect::<Vec<_>>();
            let (surfaces, prebound) = bind_all(&inputs, &paths)?;
            let surfaces: Surfaces = Arc::new(surfaces);
            let mut resolved = resolve_files(&inputs, &surfaces, &prebound, &paths)?;
            let calls = MethodCallInputs::build(&inputs, &resolved);
            for (path, sites, dependencies) in calls.resolve_sites(&inputs, &paths) {
                let file = resolved.get_mut(&path).expect("resolved");
                file.sites = sites;
                file.site_dependencies = dependencies;
            }
            Ok::<_, ManifestJoinError>((surfaces, prebound, resolved, calls))
        })?;
        Ok(Self {
            inputs,
            surfaces,
            prebound,
            resolved,
            calls,
            pool,
        })
    }

    /// Replaces the inputs with `next`, whose paths in `changed` were
    /// added, removed or rewritten, and updates the resolution.
    ///
    /// # Errors
    ///
    /// When resolution fails on a file.
    pub(crate) fn update(
        &mut self,
        next: CheckoutInputs,
        changed: &BTreeSet<String>,
    ) -> Result<UpdateStats, ManifestJoinError> {
        let started = Instant::now();
        let pool = std::mem::replace(&mut self.pool, super::callgraph::pool(1)?);
        let result = pool.install(|| self.update_in_pool(next, changed));
        self.pool = pool;
        let mut stats = result?;
        stats.total_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(stats)
    }

    fn update_in_pool(
        &mut self,
        next: CheckoutInputs,
        changed: &BTreeSet<String>,
    ) -> Result<UpdateStats, ManifestJoinError> {
        let mut stats = UpdateStats {
            changed_paths: changed.len(),
            ..UpdateStats::default()
        };
        let old = std::mem::replace(&mut self.inputs, next);
        let inputs = &self.inputs;
        let membership = changed
            .iter()
            .filter(|path| {
                old.state.get(path.as_bytes()).is_some()
                    != inputs.state.get(path.as_bytes()).is_some()
                    || old.state.get(path.as_bytes()).map(|record| record.kind)
                        != inputs.state.get(path.as_bytes()).map(|record| record.kind)
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        let configs = changed
            .iter()
            .filter(|path| {
                old.configs.contains_key(path.as_bytes())
                    || inputs.configs.contains_key(path.as_bytes())
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        let contents = changed
            .iter()
            .filter(|path| {
                old.parses.get(path.as_str()).map(|(key, _)| key)
                    != inputs.parses.get(path.as_str()).map(|(key, _)| key)
            })
            .cloned()
            .collect::<BTreeSet<_>>();

        // Pass 1. A binding depends on the file's own parse, on which paths
        // exist, and on configuration it read.
        let started = Instant::now();
        let mut binding_keys = membership.clone();
        binding_keys.extend(configs.iter().cloned());
        if !configs.is_empty() {
            binding_keys.insert(VIEW_CONFIG_MEMBERSHIP_DOMAIN.to_string());
        }
        let rebind = inputs
            .parses
            .keys()
            .filter(|path| {
                contents.contains(*path)
                    || self
                        .prebound
                        .get(path.as_str())
                        .is_none_or(|binding| !binding.dependencies.is_disjoint(&binding_keys))
            })
            .collect::<Vec<_>>();
        let (new_surfaces, new_prebound) = bind_all(inputs, &rebind)?;
        stats.rebound = rebind.len();
        let mut surfaces_changed = BTreeSet::new();
        for path in &rebind {
            let same = self
                .prebound
                .get(path.as_str())
                .is_some_and(|old| old.same_surface(&new_prebound[path.as_str()]));
            if !same {
                surfaces_changed.insert((*path).clone());
            }
        }
        for path in old.parses.keys() {
            if !inputs.parses.contains_key(path) {
                surfaces_changed.insert(path.clone());
                self.prebound.remove(path);
                self.resolved.remove(path);
            }
        }
        stats.surfaces_changed = surfaces_changed.len();
        let mut surfaces = (*self.surfaces).clone();
        surfaces.retain(|path, _| inputs.parses.contains_key(path));
        let references_changed = rebind
            .iter()
            .filter(|path| {
                self.prebound
                    .get(path.as_str())
                    .is_none_or(|old| old.references != new_prebound[path.as_str()].references)
            })
            .map(|path| (*path).clone())
            .collect::<BTreeSet<_>>();
        surfaces.extend(new_surfaces);
        self.prebound.extend(new_prebound);
        self.surfaces = Arc::new(surfaces);
        stats.bind_ms = started.elapsed().as_secs_f64() * 1000.0;

        // What a recorded answer can depend on that changed.
        let mut changed_keys = surfaces_changed.clone();
        changed_keys.extend(membership.iter().cloned());
        changed_keys.extend(configs.iter().cloned());
        if !configs.is_empty() {
            changed_keys.insert(VIEW_CONFIG_MEMBERSHIP_DOMAIN.to_string());
        }
        if surfaces_changed.iter().any(|path| path.ends_with(".rs"))
            || membership.iter().any(|path| path.ends_with(".rs"))
        {
            changed_keys.insert(VIEW_RUST_MODULE_DOMAIN.to_string());
        }
        if membership.iter().any(|path| path.ends_with(".go")) {
            changed_keys.insert(VIEW_GO_PACKAGE_DOMAIN.to_string());
        }

        // Pass 2.
        let started = Instant::now();
        let facts = Rc::new(ViewBindingFacts::new(Rc::new(CheckoutFacts { inputs })));
        let index = resolver_index(&self.surfaces, &facts);
        let mut answers = BTreeMap::new();
        let mut resolve = BTreeSet::new();
        let mut replayed = Vec::new();
        for path in inputs.parses.keys() {
            let Some(old) = self.resolved.get(path) else {
                resolve.insert(path.clone());
                continue;
            };
            if contents.contains(path) || references_changed.contains(path) {
                resolve.insert(path.clone());
                continue;
            }
            if old.binding.dependencies.is_disjoint(&changed_keys) {
                continue;
            }
            stats.replayed += 1;
            if old
                .binding
                .surface_replays(&index, &changed_keys, &mut answers)
            {
                replayed.push(path.clone());
            } else {
                resolve.insert(path.clone());
            }
        }
        drop(answers);
        // Files kept whose binding was redone carry their resolution over.
        for path in &rebind {
            if resolve.contains(*path) {
                continue;
            }
            let mut binding = self.prebound[path.as_str()].clone();
            if let Some(old) = self.resolved.get_mut(path.as_str()) {
                binding.reuse_resolution(&old.binding);
                old.binding = binding;
            }
        }
        stats.kept_after_replay = replayed.len();
        let paths = resolve.iter().collect::<Vec<_>>();
        let fresh = resolve_files(inputs, &self.surfaces, &self.prebound, &paths)?;
        stats.resolved = fresh.len();
        for (path, mut file) in fresh {
            // Pass 4 decides below whether its sites change.
            if let Some(old) = self.resolved.remove(&path) {
                file.sites = old.sites;
                file.site_dependencies = old.site_dependencies;
            }
            self.resolved.insert(path, file);
        }
        stats.resolve_ms = started.elapsed().as_secs_f64() * 1000.0;

        // Pass 3, for files whose rows were kept: replay each trace.
        let started = Instant::now();
        let mut trace_answers = BTreeMap::new();
        let mut recompute = Vec::new();
        for (path, file) in &self.resolved {
            if resolve.contains(path) {
                continue;
            }
            let Some(dispatch) = &file.dispatch else {
                continue;
            };
            let trace = &dispatch.trace;
            if trace.dependencies().is_disjoint(&changed_keys) && !trace.probes_any(&membership) {
                continue;
            }
            stats.dispatch_replayed += 1;
            if trace.probes_any(&membership)
                || !trace.replays(&index, &changed_keys, &mut trace_answers)
            {
                recompute.push(path.clone());
            }
        }
        drop(trace_answers);
        for path in &recompute {
            let (_, parse) = &inputs.parses[path.as_str()];
            let dispatch = dispatch_targets(path, parse, &index, &facts)?;
            self.resolved.get_mut(path).expect("kept").dispatch = dispatch;
        }
        stats.dispatch_recomputed = recompute.len();
        stats.dispatch_ms = started.elapsed().as_secs_f64() * 1000.0;
        drop(index);
        drop(facts);

        // Pass 4.
        let started = Instant::now();
        let mut site_keys = BTreeSet::new();
        let mut sites = contents.clone();
        let touched = resolve
            .iter()
            .chain(&recompute)
            .chain(changed)
            .cloned()
            .collect::<BTreeSet<_>>();
        for path in &touched {
            let (keys, own) = self.calls.update(inputs, path, self.resolved.get(path));
            if !keys.is_empty() {
                stats.facts_changed += 1;
            }
            site_keys.extend(keys);
            if own && inputs.parses.contains_key(path) {
                sites.insert(path.clone());
            }
        }
        for (path, file) in &self.resolved {
            if !file.site_dependencies.is_disjoint(&site_keys) {
                sites.insert(path.clone());
            }
        }
        sites.retain(|path| inputs.parses.contains_key(path));
        let paths = sites.iter().collect::<Vec<_>>();
        for (path, kept, dependencies) in self.calls.resolve_sites(inputs, &paths) {
            let file = self.resolved.get_mut(&path).expect("resolved");
            file.sites = kept;
            file.site_dependencies = dependencies;
        }
        stats.sites_resolved = paths.len();
        stats.sites_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(stats)
    }
}
