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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use crate::callgraph_store::fact_answers::{FactAnswer, FactAnswers, FactQuestion};
use crate::callgraph_store::facts::{FactPaths, ProjectFacts};
use crate::callgraph_store::join::{
    rust_module_key_for_file, ManifestJoinError, ViewBindingDependencies, ViewBindingFacts,
    VIEW_CONFIG_MEMBERSHIP_DOMAIN, VIEW_GO_MODULES_DOMAIN, VIEW_GO_PACKAGE_DOMAIN,
    VIEW_RUST_CRATES_DOMAIN, VIEW_RUST_MODULE_DOMAIN,
};
use crate::callgraph_store::DbFileIndex;
use crate::callgraph_store::{go_module_map, rust_crate_map};

use super::callgraph::{
    bind_all, dispatch_targets, resolve_files, resolver_index, CheckoutFacts, CheckoutInputs,
    FileResolution, MethodCallInputs, Surfaces,
};

/// What one update did, for measurement.
#[derive(Clone, Debug, Default)]
pub struct UpdateStats {
    pub changed_paths: usize,
    /// Recorded checkout reads asked again, and how many got a different
    /// answer.
    pub answers_checked: usize,
    pub answers_changed: usize,
    pub facts_ms: f64,
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
    /// Files whose stored resolution changed: rebound, resolved, traced or
    /// re-sited again, or removed.
    pub touched: BTreeSet<String>,
}

/// Which files recorded each input key, for one kind of recording: the
/// reverse inputs a change is looked up in.
#[derive(Debug, Default)]
struct Readers {
    by_key: HashMap<String, HashSet<String>>,
    keys_of: HashMap<String, BTreeSet<String>>,
}

impl Readers {
    fn set(&mut self, file: &str, keys: BTreeSet<String>) {
        self.remove(file);
        for key in &keys {
            self.by_key
                .entry(key.clone())
                .or_default()
                .insert(file.to_string());
        }
        self.keys_of.insert(file.to_string(), keys);
    }

    fn remove(&mut self, file: &str) {
        let Some(keys) = self.keys_of.remove(file) else {
            return;
        };
        for key in keys {
            if let Some(files) = self.by_key.get_mut(&key) {
                files.remove(file);
                if files.is_empty() {
                    self.by_key.remove(&key);
                }
            }
        }
    }

    /// The files that recorded one of `keys`.
    fn of<'k>(&self, keys: impl IntoIterator<Item = &'k String>) -> BTreeSet<String> {
        keys.into_iter()
            .filter_map(|key| self.by_key.get(key))
            .flatten()
            .cloned()
            .collect()
    }
}

/// Every reverse input of a checkout's resolution.
#[derive(Debug, Default)]
struct ReverseInputs {
    /// What each file's binding probed or read (pass 1).
    binding: Readers,
    /// What each file's resolution depends on (pass 2).
    resolution: Readers,
    /// What each file's dispatch type targets depend on (pass 3).
    dispatch: Readers,
    /// The method-call dependency keys each file's sites recorded (pass 4).
    sites: Readers,
}

impl ReverseInputs {
    fn note_prebound(&mut self, file: &str, binding: &ViewBindingDependencies) {
        self.binding.set(file, binding.dependencies.clone());
    }

    fn note_resolution(&mut self, file: &str, resolution: &FileResolution) {
        self.resolution
            .set(file, resolution.binding.dependencies.clone());
        match &resolution.dispatch {
            Some(dispatch) => {
                let mut keys = dispatch.trace.dependencies();
                keys.extend(dispatch.trace.probes().iter().cloned());
                self.dispatch.set(file, keys);
            }
            None => self.dispatch.remove(file),
        }
        self.sites.set(file, resolution.site_dependencies.clone());
    }

    fn remove(&mut self, file: &str) {
        self.binding.remove(file);
        self.resolution.remove(file);
        self.dispatch.remove(file);
        self.sites.remove(file);
    }
}

/// A checkout's resolution, with what each pass recorded.
pub struct LiveCheckout {
    pub(crate) inputs: CheckoutInputs,
    surfaces: Surfaces,
    /// Each file's binding as pass 1 left it.
    prebound: HashMap<String, ViewBindingDependencies>,
    pub(crate) resolved: BTreeMap<String, FileResolution>,
    calls: MethodCallInputs,
    reverse: ReverseInputs,
    pool: Arc<rayon::ThreadPool>,
}

/// Files whose recorded checkout reads now get different answers, by the
/// part of their resolution that made the read.
#[derive(Default)]
struct StaleAnswers {
    binding: BTreeSet<String>,
    resolution: BTreeSet<String>,
    dispatch: BTreeSet<String>,
    /// Distinct reads asked again.
    checked: usize,
    /// Distinct reads whose answer changed.
    changed: usize,
}

impl LiveCheckout {
    /// Files whose recorded reads or surface answers no longer hold in the
    /// current checkout, whatever changed: an update that left one of them
    /// unchecked missed a file it had to resolve again. Asks everything, so
    /// it is for gates, not for updates.
    pub(crate) fn stale_recordings(&self) -> Vec<String> {
        let inputs = &self.inputs;
        let checkout = CheckoutFacts { inputs };
        let facts = Rc::new(ViewBindingFacts::new(Rc::new(CheckoutFacts { inputs })));
        let index = resolver_index(&self.surfaces, &facts);
        let mut now = HashMap::<&FactQuestion, FactAnswer>::new();
        let mut surface = BTreeMap::new();
        let mut traced = BTreeMap::new();
        let mut stale = Vec::new();
        for (path, file) in &self.resolved {
            let mut holds = file.binding.answers().all(|(question, recorded)| {
                now.entry(question)
                    .or_insert_with(|| question.answer(&checkout))
                    == recorded
            });
            holds = holds && file.binding.all_surface_answers_hold(&index, &mut surface);
            if let Some(dispatch) = &file.dispatch {
                holds = holds
                    && dispatch.trace.answers.iter().all(|(question, recorded)| {
                        now.entry(question)
                            .or_insert_with(|| question.answer(&checkout))
                            == recorded
                    })
                    && dispatch.trace.all_answers_hold(&index, &mut traced);
            }
            if !holds {
                stale.push(path.clone());
            }
        }
        stale
    }

    /// Asks every distinct recorded checkout read again of `inputs`, and
    /// lists the files with a read whose answer differs.
    fn stale_answers(&self, inputs: &CheckoutInputs, configs_changed: bool) -> StaleAnswers {
        /// Whether any of `answers` differs now; each distinct read is asked
        /// once and kept in `now`.
        fn differs<'a>(
            answers: &'a FactAnswers,
            now: &mut HashMap<&'a FactQuestion, FactAnswer>,
            facts: &dyn ProjectFacts,
        ) -> bool {
            let mut any = false;
            for (question, recorded) in answers {
                let answer = now
                    .entry(question)
                    .or_insert_with(|| question.answer(facts));
                any |= answer != recorded;
            }
            any
        }
        let facts = CheckoutFacts { inputs };
        let mut now = HashMap::<&FactQuestion, FactAnswer>::new();
        let mut stale = StaleAnswers::default();
        let files = self.resolved.iter().collect::<Vec<_>>();
        for (path, file) in &files {
            // A configuration read the resolver attributed to no field is
            // recorded without its answer, so any configuration change may
            // change it.
            if differs(&file.binding.binding_answers, &mut now, &facts)
                || (configs_changed && file.binding.unattributed)
            {
                stale.binding.insert((*path).clone());
            }
            if differs(&file.binding.resolution_answers, &mut now, &facts) {
                stale.resolution.insert((*path).clone());
            }
            if let Some(dispatch) = &file.dispatch {
                if differs(&dispatch.trace.answers, &mut now, &facts) {
                    stale.dispatch.insert((*path).clone());
                }
            }
        }
        stale.checked = now.len();
        stale.changed = files
            .iter()
            .flat_map(|(_, file)| {
                file.binding
                    .answers()
                    .chain(file.dispatch.iter().flat_map(|d| d.trace.answers.iter()))
            })
            .filter(|(question, recorded)| {
                now.get(question).is_some_and(|answer| answer != *recorded)
            })
            .map(|(question, _)| question)
            .collect::<BTreeSet<_>>()
            .len();
        stale
    }

    /// Resolves every file of `inputs` cold.
    ///
    /// # Errors
    ///
    /// When resolution fails on a file.
    pub(crate) fn cold(inputs: CheckoutInputs, threads: usize) -> Result<Self, ManifestJoinError> {
        let pool = Arc::new(super::callgraph::pool(threads)?);
        let (surfaces, prebound, resolved, calls) = pool.install(|| {
            let paths = inputs.parses.keys().collect::<Vec<_>>();
            let (surfaces, prebound) = bind_all(&inputs, &paths)?;
            let surfaces: Surfaces = Arc::new(surfaces);
            let mut resolved = resolve_files(&inputs, &surfaces, &prebound, &paths)?;
            let mut calls = MethodCallInputs::build(&inputs, &resolved);
            for (path, sites, dependencies) in calls.resolve_sites(&inputs, &paths) {
                let file = resolved.get_mut(&path).expect("resolved");
                file.sites = sites;
                file.site_dependencies = dependencies;
            }
            Ok::<_, ManifestJoinError>((surfaces, prebound, resolved, calls))
        })?;
        let mut reverse = ReverseInputs::default();
        for (path, binding) in &prebound {
            reverse.note_prebound(path, binding);
        }
        for (path, file) in &resolved {
            reverse.note_resolution(path, file);
        }
        Ok(Self {
            inputs,
            surfaces,
            prebound,
            resolved,
            calls,
            reverse,
            pool,
        })
    }

    /// A checkout's resolution from what a publication stored: every file's
    /// resolution as `resolved` holds it. Binds every file again for the
    /// surfaces and pass-1 bindings later updates compare against, without
    /// resolving anything.
    ///
    /// # Errors
    ///
    /// When binding fails on a file.
    pub(crate) fn from_resolved(
        inputs: CheckoutInputs,
        resolved: BTreeMap<String, FileResolution>,
        threads: usize,
    ) -> Result<Self, ManifestJoinError> {
        Self::from_resolved_timed(inputs, resolved, threads, &mut Vec::new())
    }

    /// [`Self::from_resolved`], adding each step's time to `timings`.
    pub(crate) fn from_resolved_timed(
        inputs: CheckoutInputs,
        resolved: BTreeMap<String, FileResolution>,
        threads: usize,
        timings: &mut Vec<(&'static str, f64)>,
    ) -> Result<Self, ManifestJoinError> {
        let ms = |started: Instant| started.elapsed().as_secs_f64() * 1000.0;
        let pool = Arc::new(super::callgraph::pool(threads)?);
        let (surfaces, prebound, calls, bind_ms, calls_ms) = pool.install(|| {
            let paths = inputs.parses.keys().collect::<Vec<_>>();
            let started = Instant::now();
            let (surfaces, prebound) = bind_all(&inputs, &paths)?;
            let bind_ms = ms(started);
            let started = Instant::now();
            let calls = MethodCallInputs::build(&inputs, &resolved);
            Ok::<_, ManifestJoinError>((Arc::new(surfaces), prebound, calls, bind_ms, ms(started)))
        })?;
        timings.push(("bind every file", bind_ms));
        timings.push(("method-call inputs", calls_ms));
        let started = Instant::now();
        let mut reverse = ReverseInputs::default();
        for (path, binding) in &prebound {
            reverse.note_prebound(path, binding);
        }
        for (path, file) in &resolved {
            reverse.note_resolution(path, file);
        }
        timings.push(("reverse input index", ms(started)));
        Ok(Self {
            inputs,
            surfaces,
            prebound,
            resolved,
            calls,
            reverse,
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
        let pool = Arc::clone(&self.pool);
        let mut stats = pool.install(|| self.update_in_pool(next, changed))?;
        stats.total_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(stats)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the four passes run in order and each reads the previous one's results"
    )]
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
                old.state.get(path.as_bytes()).map(|record| record.kind)
                    != inputs.state.get(path.as_bytes()).map(|record| record.kind)
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        let link = |inputs: &CheckoutInputs, path: &str| {
            inputs
                .state
                .get(path.as_bytes())
                .filter(|record| record.kind == aft_store::checkout::PathKind::Symlink)
                .map(|record| record.link.clone())
        };
        let links = changed
            .iter()
            .filter(|path| link(&old, path) != link(inputs, path))
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
        // Configuration files added or removed. A content change of an
        // existing one reaches its readers through their recorded field
        // answers, which are asked again below; only membership changes need
        // every config reader to bind again.
        let config_membership = configs
            .intersection(&membership)
            .cloned()
            .collect::<BTreeSet<_>>();
        // The Go module map and the Rust crate map are each built from a
        // walk of the checkout, and a reader records only the map's key. The
        // key changes when the map does, so build each map before and after
        // whenever its inputs can have changed: a go.mod or Cargo.toml edit,
        // a symlink (a walk may follow it), or for crates any path added or
        // removed (a crate's `src` folder can appear).
        let is_named = |path: &String, name: &str| {
            path == name
                || path
                    .strip_suffix(name)
                    .is_some_and(|parent| parent.ends_with('/'))
        };
        let maps_differ = |build: &dyn Fn(&FactPaths<'_>) -> HashMap<String, String>| {
            let root = std::path::Path::new("/");
            let before = build(&FactPaths {
                root,
                facts: &CheckoutFacts { inputs: &old },
            });
            let after = build(&FactPaths {
                root,
                facts: &CheckoutFacts { inputs },
            });
            before != after
        };
        let go_modules_changed = (!links.is_empty()
            || configs.iter().any(|path| is_named(path, "go.mod")))
            && maps_differ(&go_module_map);
        let rust_crates_changed = (!links.is_empty()
            || !membership.is_empty()
            || configs.iter().any(|path| is_named(path, "Cargo.toml")))
            && maps_differ(&rust_crate_map);
        drop(old);

        // Every checkout read a resolution made is recorded with its answer.
        // When paths, links or configuration changed, each distinct recorded
        // read is asked again; a file any of whose answers differ is bound,
        // resolved or traced again. This needs no rule about which change can
        // affect which read.
        let started = Instant::now();
        let stale = if membership.is_empty() && links.is_empty() && configs.is_empty() {
            StaleAnswers::default()
        } else {
            self.stale_answers(inputs, !configs.is_empty())
        };
        stats.answers_checked = stale.checked;
        stats.answers_changed = stale.changed;
        stats.facts_ms = started.elapsed().as_secs_f64() * 1000.0;

        // Pass 1. A binding depends on the file's own parse, on which paths
        // exist, and on configuration it read.
        let started = Instant::now();
        let mut binding_keys = membership.clone();
        if !config_membership.is_empty() {
            binding_keys.insert(VIEW_CONFIG_MEMBERSHIP_DOMAIN.to_string());
        }
        if go_modules_changed {
            binding_keys.insert(VIEW_GO_MODULES_DOMAIN.to_string());
        }
        if rust_crates_changed {
            binding_keys.insert(VIEW_RUST_CRATES_DOMAIN.to_string());
        }
        let mut rebind = self.reverse.binding.of(&binding_keys);
        rebind.extend(contents.iter().cloned());
        rebind.extend(stale.binding.iter().cloned());
        rebind.retain(|path| inputs.parses.contains_key(path));
        let rebind = rebind.into_iter().collect::<Vec<_>>();
        let rebind_refs = rebind.iter().collect::<Vec<_>>();
        let (new_surfaces, new_prebound) = bind_all(inputs, &rebind_refs)?;
        stats.rebound = rebind.len();
        // The Rust module tree changes when a Rust file is added or removed,
        // or a rebound file declares different modules.
        let module_tree_changed = membership.iter().any(|path| path.ends_with(".rs"))
            || rebind.iter().any(|path| {
                path.ends_with(".rs")
                    && self
                        .surfaces
                        .get(path.as_str())
                        .map(DbFileIndex::declared_module_targets)
                        != new_surfaces
                            .get(path.as_str())
                            .map(DbFileIndex::declared_module_targets)
            });
        let mut surfaces_changed = BTreeSet::new();
        let mut references_changed = BTreeSet::new();
        for path in &rebind {
            let new = &new_prebound[path.as_str()];
            match self.prebound.get(path.as_str()) {
                Some(old) => {
                    if !old.same_surface(new) {
                        surfaces_changed.insert(path.clone());
                    }
                    if old.references != new.references {
                        references_changed.insert(path.clone());
                    }
                }
                None => {
                    surfaces_changed.insert(path.clone());
                    references_changed.insert(path.clone());
                }
            }
        }
        let removed = self
            .resolved
            .keys()
            .filter(|path| !inputs.parses.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        for path in &removed {
            surfaces_changed.insert(path.clone());
            self.prebound.remove(path);
            self.resolved.remove(path);
            self.reverse.remove(path);
        }
        stats.surfaces_changed = surfaces_changed.len();
        // No index from the last update is alive, so this edits in place.
        let surfaces = Arc::make_mut(&mut self.surfaces);
        for path in &removed {
            surfaces.remove(path);
        }
        surfaces.extend(new_surfaces);
        for (path, binding) in new_prebound {
            self.reverse.note_prebound(&path, &binding);
            self.prebound.insert(path, binding);
        }
        stats.bind_ms = started.elapsed().as_secs_f64() * 1000.0;

        // What a recorded answer can depend on that changed.
        let mut changed_keys = surfaces_changed.clone();
        changed_keys.extend(membership.iter().cloned());
        if !config_membership.is_empty() {
            changed_keys.insert(VIEW_CONFIG_MEMBERSHIP_DOMAIN.to_string());
        }
        if module_tree_changed {
            changed_keys.insert(VIEW_RUST_MODULE_DOMAIN.to_string());
        }
        for path in surfaces_changed.iter().filter(|path| path.ends_with(".rs")) {
            changed_keys.insert(rust_module_key_for_file(path));
        }
        if membership.iter().any(|path| path.ends_with(".go")) {
            changed_keys.insert(VIEW_GO_PACKAGE_DOMAIN.to_string());
        }
        if go_modules_changed {
            changed_keys.insert(VIEW_GO_MODULES_DOMAIN.to_string());
        }
        if rust_crates_changed {
            changed_keys.insert(VIEW_RUST_CRATES_DOMAIN.to_string());
        }

        // Pass 2.
        let started = Instant::now();
        let facts = Rc::new(ViewBindingFacts::typed(Rc::new(CheckoutFacts { inputs })));
        let index = resolver_index(&self.surfaces, &facts);
        let mut resolve = contents.clone();
        resolve.extend(references_changed.iter().cloned());
        resolve.extend(stale.binding.iter().cloned());
        resolve.extend(stale.resolution.iter().cloned());
        resolve.retain(|path| inputs.parses.contains_key(path));
        for path in inputs.parses.keys() {
            if !self.resolved.contains_key(path) {
                resolve.insert(path.clone());
            }
        }
        let mut answers = BTreeMap::new();
        for path in self.reverse.resolution.of(&changed_keys) {
            if resolve.contains(&path) {
                continue;
            }
            let Some(old) = self.resolved.get(&path) else {
                continue;
            };
            stats.replayed += 1;
            if old
                .binding
                .surface_replays(&index, &changed_keys, &mut answers)
            {
                stats.kept_after_replay += 1;
            } else {
                resolve.insert(path);
            }
        }
        drop(answers);
        // Files kept whose binding was redone carry their resolution over.
        for path in &rebind {
            if resolve.contains(path) {
                continue;
            }
            if let Some(old) = self.resolved.get_mut(path.as_str()) {
                let mut binding = self.prebound[path.as_str()].clone();
                binding.reuse_resolution(&old.binding);
                old.binding = binding;
                self.reverse.note_resolution(path, old);
            }
        }
        let paths = resolve.iter().collect::<Vec<_>>();
        let fresh = resolve_files(inputs, &self.surfaces, &self.prebound, &paths)?;
        stats.resolved = fresh.len();
        for (path, mut file) in fresh {
            // Pass 4 decides below whether its sites change.
            if let Some(old) = self.resolved.remove(&path) {
                file.sites = old.sites;
                file.site_dependencies = old.site_dependencies;
            }
            self.reverse.note_resolution(&path, &file);
            self.resolved.insert(path, file);
        }
        stats.resolve_ms = started.elapsed().as_secs_f64() * 1000.0;

        // Pass 3, for files whose rows were kept: replay each trace.
        let started = Instant::now();
        let mut trace_answers = BTreeMap::new();
        let mut recompute = Vec::new();
        let mut trace_keys = changed_keys.clone();
        trace_keys.extend(membership.iter().cloned());
        for path in &stale.dispatch {
            if !resolve.contains(path) && !recompute.contains(path) {
                recompute.push(path.clone());
            }
        }
        for path in self.reverse.dispatch.of(&trace_keys) {
            if resolve.contains(&path) {
                continue;
            }
            let Some(dispatch) = self
                .resolved
                .get(&path)
                .and_then(|file| file.dispatch.as_ref())
            else {
                continue;
            };
            let trace = &dispatch.trace;
            stats.dispatch_replayed += 1;
            if recompute.contains(&path) {
                continue;
            }
            if trace.probes_any(&membership)
                || !trace.replays(&index, &changed_keys, &mut trace_answers)
            {
                recompute.push(path);
            }
        }
        drop(trace_answers);
        for path in &recompute {
            let (_, parse) = &inputs.parses[path.as_str()];
            let dispatch = dispatch_targets(path, parse, &index, &facts)?;
            let file = self.resolved.get_mut(path).expect("kept");
            file.dispatch = dispatch;
            self.reverse.note_resolution(path, file);
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
            if own {
                sites.insert(path.clone());
            }
        }
        sites.extend(self.reverse.sites.of(&site_keys));
        sites.retain(|path| inputs.parses.contains_key(path));
        let paths = sites.iter().collect::<Vec<_>>();
        for (path, kept, dependencies) in self.calls.resolve_sites(inputs, &paths) {
            let file = self.resolved.get_mut(&path).expect("resolved");
            file.sites = kept;
            file.site_dependencies = dependencies;
            self.reverse
                .sites
                .set(&path, file.site_dependencies.clone());
        }
        stats.sites_resolved = paths.len();
        stats.sites_ms = started.elapsed().as_secs_f64() * 1000.0;
        stats.touched = rebind
            .into_iter()
            .chain(resolve)
            .chain(recompute)
            .chain(sites)
            .chain(removed)
            .collect();
        Ok(stats)
    }
}
