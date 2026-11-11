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

use crate::callgraph::js_workspace_package_map;
use crate::callgraph_store::fact_answers::{FactAnswer, FactAnswers, FactQuestion};
use crate::callgraph_store::facts::{FactPaths, ProjectFacts};
use crate::callgraph_store::join::{
    rust_crate_key, rust_module_key_for_file, ManifestJoinError, ViewBindingDependencies,
    ViewBindingFacts, VIEW_GO_MODULES_DOMAIN, VIEW_GO_PACKAGE_DOMAIN,
};
use crate::callgraph_store::{go_module_map, rust_crate_map};

/// Whether the checkout path `path` is a file called `name`.
fn is_config_named(path: &str, name: &str) -> bool {
    path == name
        || path
            .strip_suffix(name)
            .is_some_and(|parent| parent.ends_with('/'))
}

use aft_store::graph::TermKind;

use super::callgraph::{
    bind_all, dispatch_targets, resolve_files, resolver_index, BindCache, CheckoutFacts,
    CheckoutInputs, FileResolution, MethodCallInputs, Resolutions,
};
use super::graph_store::StoredSlices;
use super::refresh_budget::{RefreshBudget, RefreshLimit};
use super::subjects::{affected_by, subjects, Subject};

/// What one update did, for measurement.
#[derive(Clone, Debug, Default)]
pub struct UpdateStats {
    /// A broad incremental update was replaced by a stored-record cold build.
    pub cold_build: bool,
    pub changed_paths: usize,
    /// Recorded checkout reads asked again, and how many got a different
    /// answer.
    pub answers_checked: usize,
    pub answers_changed: usize,
    pub facts_ms: f64,
    pub rebound: usize,
    /// Why files were bound again: how many through recorded input keys, how
    /// many through a recorded checkout read whose answer changed, and the
    /// keys that reached the most files, most first.
    pub rebound_by_keys: usize,
    pub rebound_by_answers: usize,
    pub rebound_top_keys: Vec<(String, usize)>,
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

pub(crate) enum UpdateError {
    Resolve(ManifestJoinError),
    Limit(RefreshLimit),
}

impl From<ManifestJoinError> for UpdateError {
    fn from(error: ManifestJoinError) -> Self {
        Self::Resolve(error)
    }
}

fn admit<'a>(
    budget: &mut Option<&mut RefreshBudget>,
    files: impl IntoIterator<Item = &'a String>,
) -> Result<(), UpdateError> {
    if let Some(budget) = budget.as_deref_mut() {
        budget.include(files).map_err(UpdateError::Limit)?;
    }
    Ok(())
}

/// Which files recorded each input key, for one kind of recording: the
/// reverse inputs a change is looked up in.
///
/// An engine started from a publication keeps in memory only the files it
/// filed since; every other file is found through the terms the
/// publication stored for it (see `graph_store::input_terms`). A file set
/// again or removed here replaces its stored terms.
#[derive(Debug)]
struct Readers {
    kind: TermKind,
    by_key: HashMap<String, HashSet<String>>,
    keys_of: HashMap<String, BTreeSet<String>>,
    /// Files whose stored terms of this kind no longer apply.
    replaced: HashSet<String>,
}

impl Readers {
    fn new(kind: TermKind) -> Self {
        Self {
            kind,
            by_key: HashMap::new(),
            keys_of: HashMap::new(),
            replaced: HashSet::new(),
        }
    }

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
        self.replaced.insert(file.to_string());
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

    /// The files that recorded one of `keys`: those filed in memory, and
    /// those `stored` files under it that were not filed again since. A
    /// stored term is a hash, so a few extra files may be listed; every
    /// caller checks or redoes a listed file, so an extra one costs work,
    /// never correctness.
    fn of<'k>(
        &self,
        keys: impl IntoIterator<Item = &'k String>,
        stored: Option<&StoredSlices>,
    ) -> BTreeSet<String> {
        let mut files = BTreeSet::new();
        for key in keys {
            if let Some(found) = self.by_key.get(key) {
                files.extend(found.iter().cloned());
            }
            if let Some(stored) = stored {
                files.extend(
                    stored
                        .files_recording(self.kind, key.as_bytes())
                        .into_iter()
                        .filter(|file| !self.replaced.contains(file)),
                );
            }
        }
        files
    }
}

/// The key a [`Subject`] is filed under in memory: the bytes of its stored
/// term.
fn subject_key(subject: &Subject) -> String {
    String::from_utf8_lossy(&subject.term()).into_owned()
}

/// Filed under this key: files with a configuration read attributed to no
/// field, which any configuration change may change.
pub(crate) const UNATTRIBUTED_TERM: &[u8] = b"u\0";

/// Every reverse input of a checkout's resolution.
struct ReverseInputs {
    /// What each file's binding probed or read (pass 1).
    binding: Readers,
    /// What each file's resolution depends on (pass 2).
    resolution: Readers,
    /// What each file's dispatch type targets depend on (pass 3).
    dispatch: Readers,
    /// The method-call dependency keys each file's sites recorded (pass 4).
    sites: Readers,
    /// The paths and folders each file's recorded checkout reads consulted,
    /// and [`UNATTRIBUTED_TERM`] for files with an unattributed
    /// configuration read.
    subjects: Readers,
    /// The publication the engine started from, whose stored terms file
    /// every file not filed again in memory.
    stored: Option<Arc<StoredSlices>>,
}

impl ReverseInputs {
    fn new(stored: Option<Arc<StoredSlices>>) -> Self {
        Self {
            binding: Readers::new(TermKind::BindingKey),
            resolution: Readers::new(TermKind::ResolutionKey),
            dispatch: Readers::new(TermKind::DispatchKey),
            sites: Readers::new(TermKind::SiteKey),
            subjects: Readers::new(TermKind::FactSubject),
            stored,
        }
    }

    fn binding_of<'k>(&self, keys: impl IntoIterator<Item = &'k String>) -> BTreeSet<String> {
        self.binding.of(keys, self.stored.as_deref())
    }

    fn resolution_of<'k>(&self, keys: impl IntoIterator<Item = &'k String>) -> BTreeSet<String> {
        self.resolution.of(keys, self.stored.as_deref())
    }

    fn dispatch_of<'k>(&self, keys: impl IntoIterator<Item = &'k String>) -> BTreeSet<String> {
        self.dispatch.of(keys, self.stored.as_deref())
    }

    fn sites_of<'k>(&self, keys: impl IntoIterator<Item = &'k String>) -> BTreeSet<String> {
        self.sites.of(keys, self.stored.as_deref())
    }

    fn subjects_of<'k>(&self, keys: impl IntoIterator<Item = &'k String>) -> BTreeSet<String> {
        self.subjects.of(keys, self.stored.as_deref())
    }

    fn note_prebound(&mut self, file: &str, binding: &ViewBindingDependencies) {
        self.binding.set(file, binding.dependencies.clone());
    }

    fn note_resolution(
        &mut self,
        file: &str,
        resolution: &FileResolution,
        facts: &CheckoutFacts<'_>,
    ) {
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
        self.note_subjects(file, resolution, facts);
    }

    /// Files `file` under what its recorded reads consult in the checkout
    /// `facts` answers for. Called again whenever its reads are asked
    /// again: a read that still holds can consult other paths than it did
    /// (a symlink on its way now points elsewhere).
    fn note_subjects(
        &mut self,
        file: &str,
        resolution: &FileResolution,
        facts: &CheckoutFacts<'_>,
    ) {
        let questions = resolution
            .binding
            .answers()
            .map(|(question, _)| question)
            .chain(
                resolution
                    .dispatch
                    .iter()
                    .flat_map(|d| d.trace.answers.keys()),
            );
        let mut keys = BTreeSet::new();
        for question in questions {
            for subject in subjects(question, facts) {
                keys.insert(subject_key(&subject));
            }
        }
        if resolution.binding.unattributed {
            keys.insert(String::from_utf8_lossy(UNATTRIBUTED_TERM).into_owned());
        }
        self.subjects.set(file, keys);
    }

    fn remove(&mut self, file: &str) {
        self.binding.remove(file);
        self.resolution.remove(file);
        self.dispatch.remove(file);
        self.sites.remove(file);
        self.subjects.remove(file);
    }
}

/// A checkout's resolution, with what each pass recorded.
pub struct LiveCheckout {
    pub(crate) inputs: CheckoutInputs,
    /// Each file's bound surface and pass-1 binding, bound when first needed.
    bound: BindCache,
    pub(crate) resolved: Resolutions,
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
    /// Files resolved now whose terms stored in the publication this engine
    /// started from no longer apply: re-resolved, bound again, or whose
    /// recorded reads now consult other paths. Publishing what changed
    /// writes their fragments again.
    pub(crate) fn refiled(&self) -> BTreeSet<String> {
        let reverse = &self.reverse;
        [
            &reverse.binding,
            &reverse.resolution,
            &reverse.dispatch,
            &reverse.sites,
            &reverse.subjects,
        ]
        .into_iter()
        .flat_map(|readers| readers.replaced.iter())
        .filter(|file| self.resolved.contains_key(file))
        .cloned()
        .collect()
    }

    /// Checks that `stored`, the publication this engine started from,
    /// files every resolution under each input key the engine's own reverse
    /// index holds, and lists every key whose readers the stored terms miss.
    /// For gates: the engine will look changes up in the stored terms
    /// instead of keeping this index.
    pub(crate) fn stored_readers_missing(&self, stored: &StoredSlices) -> Vec<String> {
        let started = Instant::now();
        // The index a cold engine builds in memory from every file.
        let facts = CheckoutFacts {
            inputs: &self.inputs,
        };
        let mut built = ReverseInputs::new(None);
        let paths = self.inputs.parses.keys().collect::<Vec<_>>();
        if let Err(error) = self.bound.bind_missing(&self.inputs, &paths) {
            return vec![format!("binding every file failed: {error}")];
        }
        for path in paths {
            if let Some(bound) = self.bound.get(path) {
                built.note_prebound(path, &bound.binding);
            }
        }
        for (path, file) in self.resolved.iter() {
            built.note_resolution(path, file, &facts);
        }
        let built_ms = started.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        let mut lookups = 0usize;
        let mut postings = 0usize;
        let mut missing = Vec::new();
        let kinds = [
            &built.binding,
            &built.resolution,
            &built.dispatch,
            &built.sites,
            &built.subjects,
        ];
        for readers in kinds {
            for (key, files) in &readers.by_key {
                let found = stored.files_recording(readers.kind, key.as_bytes());
                lookups += 1;
                postings += found.len();
                let lost = files
                    .iter()
                    .filter(|file| !found.contains(*file))
                    .collect::<Vec<_>>();
                if !lost.is_empty() && missing.len() < 50 {
                    missing.push(format!(
                        "{:?} {key:?}: {} readers not filed, e.g. {}",
                        readers.kind,
                        lost.len(),
                        lost[0]
                    ));
                }
            }
        }
        // Measurement, for the gate's log; not a miss.
        crate::slog_info!(
            "data layer stored input check: built {built_ms:.0} ms, {lookups} lookups returning {postings} files in {:.0} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        eprintln!(
            "stored input check: built {built_ms:.0} ms, {lookups} lookups returning {postings} files in {:.0} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        missing
    }

    /// Files whose recorded reads or surface answers no longer hold in the
    /// current checkout, whatever changed: an update that left one of them
    /// unchecked missed a file it had to resolve again. Asks everything, so
    /// it is for gates, not for updates.
    pub(crate) fn stale_recordings(&self) -> Vec<String> {
        let inputs = &self.inputs;
        let checkout = CheckoutFacts { inputs };
        let facts = Rc::new(ViewBindingFacts::new(Rc::new(CheckoutFacts { inputs })));
        let (index, _files) = resolver_index(inputs, &self.bound, &facts);
        let mut now = HashMap::<&FactQuestion, FactAnswer>::new();
        let mut surface = BTreeMap::new();
        let mut traced = BTreeMap::new();
        let mut stale = Vec::new();
        for (path, file) in self.resolved.iter() {
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

    /// Asks again each recorded checkout read of the files whose reads
    /// consulted something in `changed` (or, when `configs_changed`, read a
    /// configuration without naming a field), and lists the files with a
    /// read whose answer differs. Files whose reads all hold are filed again
    /// under what those reads consult now.
    fn stale_answers(
        &mut self,
        old: &CheckoutInputs,
        changed: &BTreeSet<String>,
        configs_changed: bool,
    ) -> StaleAnswers {
        /// Whether any of `answers` differs now; each distinct read is asked
        /// once and kept in `now`.
        fn differs(
            answers: &FactAnswers,
            now: &mut HashMap<FactQuestion, FactAnswer>,
            facts: &dyn ProjectFacts,
        ) -> bool {
            let mut any = false;
            for (question, recorded) in answers {
                let answer = now
                    .entry(question.clone())
                    .or_insert_with(|| question.answer(facts));
                any |= answer != recorded;
            }
            any
        }
        let mut keys = BTreeSet::new();
        let before = CheckoutFacts { inputs: old };
        let after = CheckoutFacts {
            inputs: &self.inputs,
        };
        for path in changed {
            for subject in affected_by(path.as_bytes(), &before, &after) {
                keys.insert(subject_key(&subject));
            }
        }
        if configs_changed {
            keys.insert(String::from_utf8_lossy(UNATTRIBUTED_TERM).into_owned());
        }
        let files = self.reverse.subjects_of(&keys);
        let inputs = &self.inputs;
        let facts = CheckoutFacts { inputs };
        let mut now = HashMap::<FactQuestion, FactAnswer>::new();
        let mut stale = StaleAnswers::default();
        let mut changed_reads = BTreeSet::new();
        for path in &files {
            // Most of these files keep their resolution, so each is read
            // without keeping it decoded ([`Resolutions::read`]).
            let Some(file) = self.resolved.read(path) else {
                continue;
            };
            let mut any = false;
            // A configuration read the resolver attributed to no field is
            // recorded without its answer, so any configuration change may
            // change it.
            if differs(&file.binding.binding_answers, &mut now, &facts)
                || (configs_changed && file.binding.unattributed)
            {
                stale.binding.insert(path.clone());
                any = true;
            }
            if differs(&file.binding.resolution_answers, &mut now, &facts) {
                stale.resolution.insert(path.clone());
                any = true;
            }
            if let Some(dispatch) = &file.dispatch {
                if differs(&dispatch.trace.answers, &mut now, &facts) {
                    stale.dispatch.insert(path.clone());
                    any = true;
                }
            }
            // Every read of this file is in `now` by here.
            changed_reads.extend(
                file.binding
                    .answers()
                    .chain(file.dispatch.iter().flat_map(|d| d.trace.answers.iter()))
                    .filter(|(question, recorded)| {
                        now.get(*question).is_some_and(|answer| answer != *recorded)
                    })
                    .map(|(question, _)| question.clone()),
            );
            if !any {
                self.reverse.note_subjects(path, &file, &facts);
            }
        }
        stale.checked = now.len();
        stale.changed = changed_reads.len();
        stale
    }

    /// Resolves every file of `inputs` cold.
    ///
    /// # Errors
    ///
    /// When resolution fails on a file.
    pub(crate) fn cold(inputs: CheckoutInputs) -> Result<Self, ManifestJoinError> {
        let pool = super::callgraph::engine_pool()?;
        let bound = BindCache::default();
        let (resolved, calls) = crate::alloc_trace::install(&pool, || {
            let paths = inputs.parses.keys().collect::<Vec<_>>();
            bound.bind_missing(&inputs, &paths)?;
            let mut resolved = resolve_files(&inputs, &bound, &paths)?;
            let mut calls = MethodCallInputs::build(&inputs, &resolved);
            for (path, sites, dependencies) in calls.resolve_sites(&inputs, &resolved, &paths)? {
                let file = resolved.get_mut(&path).expect("resolved");
                file.sites = sites;
                file.site_dependencies = dependencies;
            }
            Ok::<_, ManifestJoinError>((resolved, calls))
        })?;
        let mut reverse = ReverseInputs::new(None);
        let facts = CheckoutFacts { inputs: &inputs };
        for path in inputs.parses.keys() {
            let bound = bound.get(path).expect("every parsed file was bound");
            reverse.note_prebound(path, &bound.binding);
        }
        for (path, file) in &resolved {
            reverse.note_resolution(path, file, &facts);
        }
        Ok(Self {
            inputs,
            bound,
            resolved: resolved.into(),
            calls,
            reverse,
            pool,
        })
    }

    /// A checkout's resolution from what a publication stored: every file's
    /// resolution as `resolved` holds it, adding each step's time to
    /// `timings`. Binds nothing: a file is bound when an update or a
    /// resolution first needs it. The files' reverse inputs are read from
    /// `stored`'s terms, not built.
    ///
    /// # Errors
    ///
    /// When the process's engine pool cannot start.
    pub(crate) fn from_resolved_timed(
        inputs: CheckoutInputs,
        resolved: Resolutions,
        stored: Arc<StoredSlices>,
        timings: &mut Vec<(&'static str, f64)>,
    ) -> Result<Self, ManifestJoinError> {
        let started = Instant::now();
        let pool = super::callgraph::engine_pool()?;
        timings.push(("thread pool", started.elapsed().as_secs_f64() * 1000.0));
        let calls = MethodCallInputs::stored(Arc::clone(&stored));
        let reverse = ReverseInputs::new(Some(stored));
        Ok(Self {
            inputs,
            bound: BindCache::default(),
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
        self.update_with_budget(next, changed, None)
            .map_err(|error| match error {
                UpdateError::Resolve(error) => error,
                UpdateError::Limit(_) => {
                    unreachable!("an unbounded update cannot hit a refresh limit")
                }
            })
    }

    /// A limit aborts before the newly invalidated pass runs. The caller must
    /// drop this partly updated engine before rebuilding the checkout.
    pub(crate) fn update_with_budget(
        &mut self,
        next: CheckoutInputs,
        changed: &BTreeSet<String>,
        budget: Option<&mut RefreshBudget>,
    ) -> Result<UpdateStats, UpdateError> {
        let started = Instant::now();
        let pool = Arc::clone(&self.pool);
        let mut stats = crate::alloc_trace::install(&pool, || {
            let stats = self.update_in_pool(next, changed, budget)?;
            #[cfg(feature = "test-hooks")]
            super::refresh_probe::stage("encode");
            // Every resolution the update made or changed is encoded, so
            // that it can be let go between updates like a stored one.
            let parses = &self.inputs.parses;
            self.resolved.encode_changed(|path| parses.key(path));
            Ok::<_, UpdateError>(stats)
        })?;
        stats.total_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(stats)
    }

    /// This engine moved onto `stored`, a publication of exactly what it
    /// resolved, over `state`: the engine a start from that publication
    /// gives ([`Self::from_resolved_timed`]), keeping what it bound and
    /// computed so far. A file's binding and its method-call inputs follow
    /// from its parse, the checkout's paths and its configuration, all
    /// unchanged, so each stays as the new engine would compute it; every
    /// file is found through the input terms the publication stored for it
    /// again (the hashed keys each file's fragment is filed under, see
    /// `graph_store`), which file it as the engine's own index did. `Err`
    /// says why the two differ (other
    /// paths, files or parse keys, or an engine not started from a
    /// publication); the caller then starts a new engine instead.
    pub(crate) fn rebased(
        self,
        state: aft_store::checkout::CheckoutState,
        stored: Arc<StoredSlices>,
    ) -> Result<Self, &'static str> {
        if self.reverse.stored.is_none() {
            return Err("the engine was not started from a publication");
        }
        let ours = &self.inputs.state;
        if state.git != ours.git
            || state.conversion != ours.conversion
            || state.paths() != ours.paths()
        {
            return Err("the publication's paths differ from the engine's");
        }
        let parses = self
            .inputs
            .parses
            .rebased(&stored)
            .ok_or("the publication's parsed files or parse keys differ from the engine's")?;
        let resolved = self
            .resolved
            .rebased(&stored)
            .ok_or("the publication's resolved files differ from the engine's")?;
        let calls = self
            .calls
            .rebased(Arc::clone(&stored))
            .ok_or("the engine was not started from a publication")?;
        Ok(Self {
            inputs: CheckoutInputs {
                state,
                parses,
                configs: self.inputs.configs,
            },
            bound: self.bound,
            resolved,
            calls,
            reverse: ReverseInputs::new(Some(stored)),
            pool: self.pool,
        })
    }

    /// Runs `op` on the engine's thread pool, which every engine of the
    /// process shares (see `callgraph::engine_pool`).
    pub(crate) fn install<R: Send>(&self, op: impl FnOnce() -> R + Send) -> R {
        crate::alloc_trace::install(&self.pool, op)
    }

    /// Whether this engine and `other` resolve on the same thread pool.
    #[cfg(test)]
    pub(crate) fn shares_pool_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.pool, &other.pool)
    }

    /// The files resolution itself bound, and the files whose method-call
    /// inputs it computed itself, each the first time it read them, since
    /// the last call: the serial work an update did on a resolving thread.
    #[cfg(test)]
    pub(crate) fn take_first_reads(&self) -> (Vec<String>, Vec<String>) {
        (
            self.bound.take_bound_on_first_use(),
            self.calls.take_computed_while_resolving(),
        )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the four passes run in order and each reads the previous one's results"
    )]
    fn update_in_pool(
        &mut self,
        next: CheckoutInputs,
        changed: &BTreeSet<String>,
        mut budget: Option<&mut RefreshBudget>,
    ) -> Result<UpdateStats, UpdateError> {
        admit(&mut budget, changed)?;
        let mut stats = UpdateStats {
            changed_paths: changed.len(),
            ..UpdateStats::default()
        };
        let old = std::mem::replace(&mut self.inputs, next);
        #[cfg(feature = "test-hooks")]
        let mut profile_clock = Instant::now();
        #[cfg(feature = "test-hooks")]
        self.profile_refresh("start", &mut profile_clock);
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
        // Parsed files no longer parsed: removed, or no longer extractable.
        let removed_parses = changed
            .iter()
            .filter(|path| {
                old.parses.contains_key(path.as_str()) && !inputs.parses.contains_key(path.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        // Configuration files added or removed. A content change of an
        // existing one reaches its readers through their recorded field
        // answers, which are asked again below. Adding or removing one
        // reaches the files that probed its path, by their recorded answers
        // and (for one they found) their recorded path, and the crate root
        // answers that name it. A JavaScript workspace package lookup reads
        // every member's manifest, so its readers record the looked-up name's
        // key instead (see `js_package_key`), set below for each name whose
        // member changed.
        // The Go module map and the Rust crate map are each built from a
        // walk of the checkout, and a reader records keys for the map, not
        // the walk. Build each map before and after whenever its inputs can
        // have changed: a go.mod or Cargo.toml edit, a symlink (a walk may
        // follow it), or for crates any path added or removed (a crate's
        // `src` folder can appear). A Go reader records the module map's one
        // key; a Rust reader records the key of each crate name it looked
        // up, so only the names whose entry changed are set.
        let is_named = |path: &String, name: &str| is_config_named(path, name);
        let maps = |build: &dyn Fn(&FactPaths<'_>) -> HashMap<String, String>| {
            let root = std::path::Path::new("/");
            let before = build(&FactPaths {
                root,
                facts: &CheckoutFacts { inputs: &old },
            });
            let after = build(&FactPaths {
                root,
                facts: &CheckoutFacts { inputs },
            });
            (before, after)
        };
        // Each workspace package name whose resolved member changed: a
        // manifest added, removed or edited (its name, or a root's member
        // list) or a symlink changed. Every folder holding a manifest before
        // or after is a candidate workspace root.
        let changed_packages = if !links.is_empty()
            || configs
                .iter()
                .any(|path| is_named(path, "package.json") || is_named(path, "pnpm-workspace.yaml"))
        {
            let candidates = old
                .configs
                .keys()
                .chain(inputs.configs.keys())
                .filter_map(|path| std::str::from_utf8(path).ok())
                .filter_map(|path| {
                    let name = if is_config_named(path, "package.json") {
                        "package.json"
                    } else if is_config_named(path, "pnpm-workspace.yaml") {
                        "pnpm-workspace.yaml"
                    } else {
                        return None;
                    };
                    Some(
                        std::path::Path::new("/")
                            .join(path[..path.len() - name.len()].trim_end_matches('/')),
                    )
                })
                .collect::<BTreeSet<_>>();
            let (before, after) =
                maps(&|facts| js_workspace_package_map(facts, candidates.iter().cloned()));
            before
                .keys()
                .chain(after.keys())
                .filter(|key| before.get(*key) != after.get(*key))
                .cloned()
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        let go_modules_changed =
            (!links.is_empty() || configs.iter().any(|path| is_named(path, "go.mod"))) && {
                let (before, after) = maps(&go_module_map);
                before != after
            };
        // Each crate name added, removed, or whose source folder moved.
        let changed_crates = if !links.is_empty()
            || !membership.is_empty()
            || configs.iter().any(|path| is_named(path, "Cargo.toml"))
        {
            let (before, after) = maps(&rust_crate_map);
            before
                .keys()
                .chain(after.keys())
                .filter(|name| before.get(*name) != after.get(*name))
                .map(|name| rust_crate_key(name))
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };

        // Every checkout read a resolution made is recorded with its answer.
        // When paths, links or configuration changed, each distinct recorded
        // read is asked again; a file any of whose answers differ is bound,
        // resolved or traced again. This needs no rule about which change can
        // affect which read.
        #[cfg(feature = "test-hooks")]
        super::refresh_probe::stage("facts");
        let started = Instant::now();
        let stale = if membership.is_empty() && links.is_empty() && configs.is_empty() {
            StaleAnswers::default()
        } else {
            self.stale_answers(&old, changed, !configs.is_empty())
        };
        let inputs = &self.inputs;
        stats.answers_checked = stale.checked;
        stats.answers_changed = stale.changed;
        stats.facts_ms = started.elapsed().as_secs_f64() * 1000.0;
        #[cfg(feature = "test-hooks")]
        self.profile_refresh("facts", &mut profile_clock);

        // Pass 1. A binding depends on the file's own parse, on which paths
        // exist, and on configuration it read.
        #[cfg(feature = "test-hooks")]
        super::refresh_probe::stage("binding");
        let started = Instant::now();
        let mut binding_keys = membership.clone();
        binding_keys.extend(changed_packages.iter().cloned());
        if go_modules_changed {
            binding_keys.insert(VIEW_GO_MODULES_DOMAIN.to_string());
        }
        binding_keys.extend(changed_crates.iter().cloned());
        let mut rebind = self.reverse.binding_of(&binding_keys);
        stats.rebound_by_keys = rebind.len();
        stats.rebound_by_answers = stale.binding.len();
        if rebind.len() > 100 {
            let mut by_key = binding_keys
                .iter()
                .map(|key| {
                    let reached = self.reverse.binding_of(&BTreeSet::from([key.clone()]));
                    (key.replace('\0', "\\0"), reached.len())
                })
                .filter(|(_, reached)| *reached > 0)
                .collect::<Vec<_>>();
            by_key.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            by_key.truncate(5);
            stats.rebound_top_keys = by_key;
        }
        rebind.extend(contents.iter().cloned());
        rebind.extend(stale.binding.iter().cloned());
        rebind.retain(|path| inputs.parses.contains_key(path));
        admit(&mut budget, rebind.iter().chain(&removed_parses))?;
        let rebind = rebind.into_iter().collect::<Vec<_>>();
        let rebind_refs = rebind.iter().collect::<Vec<_>>();
        let new_bound = bind_all(inputs, &rebind_refs)?;
        stats.rebound = rebind.len();
        // What each rebound file was bound to before: its cached binding,
        // which holds for the old inputs, or for a file never bound since
        // the engine started, a binding under the old inputs now.
        let unbound = rebind
            .iter()
            .chain(&removed_parses)
            .filter(|path| self.bound.get(path).is_none() && old.parses.contains_key(path))
            .collect::<Vec<_>>();
        let mut old_bound = bind_all(&old, &unbound)?
            .into_iter()
            .map(|(path, bound)| (path, Arc::new(bound)))
            .collect::<HashMap<_, _>>();
        for path in rebind.iter().chain(&removed_parses) {
            if let Some(bound) = self.bound.get(path) {
                old_bound.insert(path.clone(), bound);
            }
        }
        // A parent lookup records the target file's key, including when it
        // found no parent. Only targets of changed declarations can get a
        // different parent, so only their keys are set, never one key that
        // every Rust caller records. Include removed declaring files and
        // both sides of a path override
        // or rename, even if another declaration still names the target.
        let mut changed_parents = BTreeSet::new();
        for path in rebind.iter().chain(&removed_parses) {
            let before = old_bound
                .get(path)
                .map(|bound| bound.surface.declared_module_targets());
            let after = new_bound
                .get(path)
                .map(|bound| bound.surface.declared_module_targets());
            if before != after {
                for modules in before.into_iter().chain(after) {
                    for (name, target) in modules {
                        if before.and_then(|modules| modules.get(name))
                            != after.and_then(|modules| modules.get(name))
                        {
                            changed_parents.extend(target.iter().cloned());
                        }
                    }
                }
            }
        }
        let mut surfaces_changed = BTreeSet::new();
        let mut surface_keys = BTreeSet::new();
        let mut references_changed = BTreeSet::new();
        for path in &rebind {
            let new = &new_bound[path.as_str()].binding;
            match old_bound.get(path) {
                Some(old) => {
                    if !old.binding.same_surface(new) {
                        surfaces_changed.insert(path.clone());
                        surface_keys.extend(old.binding.changed_surface_keys(new, path));
                    }
                    if old.binding.references != new.references {
                        references_changed.insert(path.clone());
                    }
                }
                None => {
                    surfaces_changed.insert(path.clone());
                    surface_keys.insert(path.clone());
                    references_changed.insert(path.clone());
                }
            }
        }
        drop(old_bound);
        let removed = removed_parses;
        for path in &removed {
            surfaces_changed.insert(path.clone());
            surface_keys.insert(path.clone());
            self.bound.remove(path);
            self.calls.remember(
                path,
                old.parses.get(path).map(|(_, parse)| &**parse),
                self.resolved.get(path),
            );
            self.resolved.remove(path);
            self.reverse.remove(path);
        }
        stats.surfaces_changed = surfaces_changed.len();
        // The whole-checkout file lists change only for the files whose
        // parse changed, appeared or went away.
        self.bound
            .update_lists(&self.inputs, contents.iter().chain(&membership));
        for (path, bound) in new_bound {
            self.reverse.note_prebound(&path, &bound.binding);
            self.bound.set(path, bound);
        }
        stats.bind_ms = started.elapsed().as_secs_f64() * 1000.0;
        #[cfg(feature = "test-hooks")]
        self.profile_refresh("bind", &mut profile_clock);

        // What a recorded answer can depend on that changed.
        let mut changed_keys = surface_keys.clone();
        changed_keys.extend(membership.iter().cloned());
        changed_keys.extend(changed_packages);
        changed_keys.extend(changed_parents);
        // Inline lookups record each candidate module-path prefix. A Rust
        // file's addition or removal changes that prefix's candidates just
        // as a content edit changes what those candidates contain.
        for path in surface_keys.iter().filter(|path| path.ends_with(".rs")) {
            changed_keys.insert(rust_module_key_for_file(path));
        }
        if membership.iter().any(|path| path.ends_with(".go")) {
            changed_keys.insert(VIEW_GO_PACKAGE_DOMAIN.to_string());
        }
        if go_modules_changed {
            changed_keys.insert(VIEW_GO_MODULES_DOMAIN.to_string());
        }
        changed_keys.extend(changed_crates);

        // Pass 2.
        #[cfg(feature = "test-hooks")]
        super::refresh_probe::stage("replay");
        let started = Instant::now();
        let facts = Rc::new(ViewBindingFacts::typed(Rc::new(CheckoutFacts { inputs })));
        let (index, files) = resolver_index(inputs, &self.bound, &facts);
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
        admit(&mut budget, &resolve)?;
        // Resolving a Rust file reads the module tree, which the resolver
        // builds by binding every module-declaring file; bind those in
        // parallel first (see `BindCache::bind_module_declaring`).
        if resolve
            .iter()
            .chain(changed)
            .any(|path| path.ends_with(".rs"))
        {
            self.bound.bind_module_declaring(inputs)?;
        }
        // Most files a change reaches here keep their resolution: each file
        // whose surface answers (pass 2) or dispatch trace (pass 3) depend on
        // something that changed is replayed. Each is read once for both,
        // in path order, without keeping it decoded ([`Resolutions::read`]).
        // Decoding them in parallel instead took a fifth more CPU, more
        // still with several checkouts refreshing at once. A file's trace
        // is replayed only if its own surface replay keeps its resolution;
        // nothing else the two passes do in between changes what a trace
        // reads (see pass 3 below).
        let mut trace_keys = changed_keys.clone();
        trace_keys.extend(membership.iter().cloned());
        let replaying = self
            .reverse
            .resolution_of(&changed_keys)
            .into_iter()
            .filter(|path| !resolve.contains(path))
            .collect::<BTreeSet<_>>();
        let tracing = self
            .reverse
            .dispatch_of(&trace_keys)
            .into_iter()
            .filter(|path| !resolve.contains(path))
            .collect::<BTreeSet<_>>();
        let mut answers = BTreeMap::new();
        let mut trace_answers = BTreeMap::new();
        let mut traces_changed = BTreeSet::new();
        for path in replaying.union(&tracing) {
            let Some(file) = self.resolved.read(path) else {
                continue;
            };
            if replaying.contains(path) {
                stats.replayed += 1;
                if file
                    .binding
                    .surface_replays(&index, &changed_keys, &mut answers)
                {
                    stats.kept_after_replay += 1;
                } else {
                    resolve.insert(path.clone());
                    continue;
                }
            }
            if !tracing.contains(path) {
                continue;
            }
            let Some(dispatch) = &file.dispatch else {
                continue;
            };
            stats.dispatch_replayed += 1;
            // A file whose recorded reads changed is traced again anyway.
            if stale.dispatch.contains(path) {
                continue;
            }
            let trace = &dispatch.trace;
            if trace.probes_any(&membership)
                || !trace.replays(&index, &changed_keys, &mut trace_answers)
            {
                traces_changed.insert(path.clone());
            }
        }
        drop(answers);
        drop(trace_answers);
        admit(&mut budget, &resolve)?;
        #[cfg(feature = "test-hooks")]
        self.profile_refresh("replay", &mut profile_clock);
        #[cfg(feature = "test-hooks")]
        super::refresh_probe::stage("resolution");
        // Files kept whose binding was redone carry their resolution over.
        for path in &rebind {
            if resolve.contains(path) {
                continue;
            }
            if let Some(old) = self.resolved.get_mut(path.as_str()) {
                let mut binding = self
                    .bound
                    .get(path)
                    .expect("rebound just now")
                    .binding
                    .clone();
                binding.reuse_resolution(&old.binding);
                // As a resolution built in memory: no surface (see
                // `callgraph::resolve_files`).
                binding.clear_surface();
                old.binding = binding;
                self.reverse
                    .note_resolution(path, old, &CheckoutFacts { inputs });
            }
        }
        let paths = resolve.iter().collect::<Vec<_>>();
        let fresh = resolve_files(inputs, &self.bound, &paths)?;
        stats.resolved = fresh.len();
        for (path, mut file) in fresh {
            self.calls.remember(
                &path,
                old.parses.get(&path).map(|(_, parse)| &**parse),
                self.resolved.get(&path),
            );
            // Pass 4 decides below whether its sites change.
            if let Some(old) = self.resolved.remove(&path) {
                file.sites = old.sites;
                file.site_dependencies = old.site_dependencies;
            }
            self.reverse
                .note_resolution(&path, &file, &CheckoutFacts { inputs });
            self.resolved.insert(path, file);
        }
        stats.resolve_ms = started.elapsed().as_secs_f64() * 1000.0;
        #[cfg(feature = "test-hooks")]
        self.profile_refresh("resolve", &mut profile_clock);

        // Pass 3, for files whose rows were kept: each trace was replayed
        // with its file's surface answers above. Resolving files again
        // changes no surface a trace reads, since every file whose binding
        // could change was bound again in pass 1.
        #[cfg(feature = "test-hooks")]
        super::refresh_probe::stage("dispatch");
        let started = Instant::now();
        let mut recompute = Vec::new();
        // Stale answers were asked before this update removed files; a
        // removed file has nothing left to recompute.
        for path in &stale.dispatch {
            if !resolve.contains(path)
                && !recompute.contains(path)
                && self.resolved.contains_key(path)
            {
                recompute.push(path.clone());
            }
        }
        recompute.extend(traces_changed);
        admit(&mut budget, &recompute)?;
        for path in &recompute {
            let (_, parse) = &inputs.parses[path.as_str()];
            let dispatch = dispatch_targets(path, parse, &index, &facts)?;
            self.calls.remember(
                path,
                old.parses.get(path).map(|(_, parse)| &**parse),
                self.resolved.get(path),
            );
            let file = self.resolved.get_mut(path).ok_or_else(|| {
                ManifestJoinError::Parse(format!("stored resolution of {path} could not be read"))
            })?;
            file.dispatch = dispatch;
            self.reverse
                .note_resolution(path, file, &CheckoutFacts { inputs });
        }
        stats.dispatch_recomputed = recompute.len();
        stats.dispatch_ms = started.elapsed().as_secs_f64() * 1000.0;
        // A file bound on first use inside passes 2 and 3 cannot return its
        // error there.
        if let Some(error) = files.take_error() {
            return Err(error.into());
        }
        drop(index);
        drop(facts);
        #[cfg(feature = "test-hooks")]
        self.profile_refresh("dispatch", &mut profile_clock);

        // Pass 4.
        #[cfg(feature = "test-hooks")]
        super::refresh_probe::stage("sites");
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
        sites.extend(self.reverse.sites_of(&site_keys));
        sites.retain(|path| inputs.parses.contains_key(path));
        admit(&mut budget, &sites)?;
        let paths = sites.iter().collect::<Vec<_>>();
        for (path, kept, dependencies) in
            self.calls.resolve_sites(inputs, &self.resolved, &paths)?
        {
            let file = self.resolved.get_mut(&path).ok_or_else(|| {
                ManifestJoinError::Parse(format!("stored resolution of {path} could not be read"))
            })?;
            file.sites = kept;
            file.site_dependencies = dependencies;
            self.reverse
                .sites
                .set(&path, file.site_dependencies.clone());
        }
        stats.sites_resolved = paths.len();
        stats.sites_ms = started.elapsed().as_secs_f64() * 1000.0;
        #[cfg(feature = "test-hooks")]
        self.profile_refresh("sites", &mut profile_clock);
        stats.touched = rebind
            .into_iter()
            .chain(resolve)
            .chain(recompute)
            .chain(sites)
            .chain(removed)
            .collect();
        Ok(stats)
    }

    #[cfg(feature = "test-hooks")]
    fn profile_refresh(&self, stage: &str, clock: &mut Instant) {
        if std::env::var_os("AFT_REFRESH_PROFILE").is_none() {
            return;
        }
        let wall_ms = clock.elapsed().as_millis();
        let cpu = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|line| {
                let rest = line.get(line.rfind(')')? + 2..)?;
                let fields = rest.split_whitespace().collect::<Vec<_>>();
                Some(fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?)
            });
        let parts = self.memory_parts(&mut aft_store::heap::Seen::new());
        log::info!("refresh profile {stage}: wall_ms={wall_ms} cpu_ticks={cpu:?} decoded_parses={} decoded_resolutions={} parts={parts:?} resolution_parts={:?}", self.inputs.parses.decoded_count(), self.resolved.decoded_count(), self.resolved.profile_parts());
        *clock = Instant::now();
    }
}

aft_store::heap_bytes_of_fields!(Readers {
    kind,
    by_key,
    keys_of,
    replaced,
});
aft_store::heap_bytes_of_fields!(ReverseInputs {
    binding,
    resolution,
    dispatch,
    sites,
    subjects,
    stored,
});

impl LiveCheckout {
    /// What the engine holds in memory, measured as the [`aft_store::heap`]
    /// module describes: the bytes of its copy of the checkout's state, and
    /// the bytes of everything else (the parses and resolutions decoded so
    /// far, the bound files, and the inputs and reverse inputs of each
    /// pass). Its thread pool owns threads, not heap, and is not counted.
    pub(crate) fn memory(&self, seen: &mut aft_store::heap::Seen) -> (usize, usize) {
        use aft_store::heap::HeapBytes as _;
        let Self {
            inputs:
                CheckoutInputs {
                    state,
                    parses,
                    configs,
                },
            bound,
            resolved,
            calls,
            reverse,
            pool: _,
        } = self;
        let state = state.heap_bytes(seen);
        let rest = parses.heap_bytes(seen)
            + configs.heap_bytes(seen)
            + bound.heap_bytes(seen)
            + resolved.heap_bytes(seen)
            + calls.heap_bytes(seen)
            + reverse.heap_bytes(seen);
        (state, rest)
    }

    /// [`Self::memory`] part by part, for finding what an update holds:
    /// each part counts the allocations no earlier part reached.
    pub(crate) fn memory_parts(
        &self,
        seen: &mut aft_store::heap::Seen,
    ) -> Vec<(&'static str, usize)> {
        use aft_store::heap::HeapBytes as _;
        let Self {
            inputs:
                CheckoutInputs {
                    state,
                    parses,
                    configs,
                },
            bound,
            resolved,
            calls,
            reverse,
            pool: _,
        } = self;
        vec![
            ("engine_state", state.heap_bytes(seen)),
            ("engine_parses", parses.heap_bytes(seen)),
            ("engine_configs", configs.heap_bytes(seen)),
            ("engine_bound", bound.heap_bytes(seen)),
            ("engine_resolutions", resolved.heap_bytes(seen)),
            ("engine_method_calls", calls.heap_bytes(seen)),
            ("engine_reverse_inputs", reverse.heap_bytes(seen)),
        ]
    }
}
