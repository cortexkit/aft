use super::*;
#[cfg(feature = "test-hooks")]
use crate::data_layer::refresh_probe;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use aft_store::graph::GraphSectionsSpill;
use aft_store::segment::ErrorKind;
use aft_store::spill::{self, WriteGuard};
use std::io;

use crate::data_layer::graph_store::SectionsError;

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct RefreshCounters {
    refreshes: AtomicU64,
    guard_fallbacks: AtomicU64,
    precheck_fallbacks: AtomicU64,
}

pub(super) fn graph_spill_config() -> spill::Config {
    let mut config = spill::Config::new(256 << 20);
    // A wide merge lets ordinary large builds sort the 25-byte term records
    // without rewriting them in intermediate runs, within the fixed budget.
    config.fan_in = 128;
    #[cfg(feature = "callgraph-shape-census")]
    if let Some(bytes) = std::env::var("AFT_GRAPH_SPILL_BUDGET_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    {
        config.memory_budget = bytes.max(1 << 20);
    }
    config
}

pub(super) struct DeltaScratch(Arc<aft_store::family::BuildScratch<DataError>>);

// A delta cannot resume a spill. Close its files, then remove the workspace
// even when a guard stops the write before the manifest commit.
impl Drop for DeltaScratch {
    fn drop(&mut self) {
        if let Err(error) = self.0.remove() {
            log::warn!(
                "could not remove delta workspace {}: {error}",
                self.0.path().display()
            );
        }
    }
}

impl DeltaScratch {
    pub(super) fn new(
        session: &mut aft_store::family::WriteSession<'_>,
        checkout: CheckoutId,
        dir: &Path,
    ) -> Result<Self, DataError> {
        let dir = dir.to_path_buf();
        session
            .create_scratch(checkout, move |reserve| check_disk_floor_at(&dir, reserve))
            .map(|scratch| Self(Arc::new(scratch)))
            .map_err(DataError::store)
    }

    pub(super) fn remove(self) -> Result<(), DataError> {
        self.0.remove().map_err(DataError::store)
    }

    pub(super) fn spill(
        &self,
        files: usize,
        estimate: u64,
        memory: Arc<RefreshMemory>,
    ) -> Result<GraphSectionsSpill, DeltaError> {
        let scratch = Arc::clone(&self.0);
        let workspace = Box::new(scratch.subdir("graph").map_err(DeltaError::io)?);
        let reserve = Arc::new(AtomicU64::new(estimate));
        let remaining = Arc::clone(&reserve);
        let guard = WriteGuard::new(move || {
            memory.check().map_err(io::Error::other)?;
            #[cfg(test)]
            publication_tests::delta_spill_check()?;
            scratch
                .check_space(remaining.load(Relaxed))
                .map_err(io::Error::other)
        });
        let mut spill = GraphSectionsSpill::new(workspace, graph_spill_config(), guard)
            .map_err(SectionsError::Spill)
            .map_err(DeltaError::finish)?;
        spill.estimate_space(files, estimate, reserve);
        Ok(spill)
    }
}

pub(super) enum DeltaError {
    Data(DataError),
    Memory(MemoryLimit),
}

impl From<DataError> for DeltaError {
    fn from(error: DataError) -> Self {
        Self::Data(error)
    }
}

impl From<RefreshStop> for DeltaError {
    fn from(stop: RefreshStop) -> Self {
        match stop {
            RefreshStop::Cancelled => Self::Data(DataError::Cancelled),
            RefreshStop::Memory(limit) => Self::Memory(limit),
        }
    }
}

impl DeltaError {
    fn guard_io(error: &io::Error) -> Option<Self> {
        let cause = error.get_ref()?;
        if let Some(stop) = cause.downcast_ref::<RefreshStop>() {
            return Some((*stop).into());
        }
        if let Some(error) = cause.downcast_ref::<DataError>() {
            return Some(error.clone().into());
        }
        // Sorter writes and merges wrap their I/O errors before the graph
        // writer in turn wraps the sorter error in a segment error.
        match cause.downcast_ref::<spill::Error>() {
            Some(spill::Error::Io(error)) => Self::guard_io(error),
            _ => None,
        }
    }

    pub(super) fn io(error: io::Error) -> Self {
        Self::guard_io(&error).unwrap_or_else(|| DataError::store(error).into())
    }

    pub(super) fn add(error: SectionsError) -> Self {
        match error {
            SectionsError::Input(reason) => DataError::Damaged(reason).into(),
            other => Self::finish(other),
        }
    }

    pub(super) fn finish(error: SectionsError) -> Self {
        let io = match &error {
            SectionsError::Spill(spill::Error::Io(error)) => Some(error),
            SectionsError::Segment(error) => match error.kind() {
                ErrorKind::Io(error) => Some(error),
                _ => None,
            },
            _ => None,
        };
        io.and_then(Self::guard_io)
            .unwrap_or_else(|| DataError::Store(error.to_string()).into())
    }
}

pub(super) fn delta_estimate<'a>(
    state: &CheckoutState,
    files: impl Iterator<Item = &'a String>,
) -> u64 {
    files
        .filter_map(|file| state.get(file.as_bytes()))
        .fold(0u64, |bytes, record| bytes.saturating_add(record.stat.size))
        .saturating_mul(8)
}

pub(super) fn delta_contents(
    changes: &BTreeMap<Vec<u8>, Change>,
    text: &TextOverlay,
    max_indexed_len: u64,
) -> (ContentSectionsBuilder, TextSectionsBuilder) {
    let mut records = ContentSectionsBuilder::new();
    let mut text_units = TextSectionsBuilder::new();
    let mut seen = HashSet::new();
    for (path, change) in changes {
        let Some(record) = &change.record else {
            continue;
        };
        let key = crate::data_layer::content::record_key(record.content, &record.language);
        match change.content.get() {
            Extracted::Parse(_, _, stored)
            | Extracted::Config(_, stored)
            | Extracted::Markers(stored) => {
                records.add_encoded(key, Arc::clone(stored));
            }
            Extracted::GrammarGap(_) | Extracted::Nothing | Extracted::ConversionFailed => {}
        }
        if !seen.insert(record.content) {
            continue;
        }
        let size = record.stat.size;
        match text.get(path) {
            Some(OverlayEntry::Content(trigrams)) => {
                text_units.add_indexed(record.content, size, trigrams);
            }
            Some(OverlayEntry::Unindexed) => match record.kind {
                PathKind::Binary => {
                    text_units.add_unindexed(record.content, size, TextStatus::Binary);
                }
                PathKind::Large => {
                    text_units.add_unindexed(record.content, size, TextStatus::Oversize);
                }
                PathKind::Text if size > max_indexed_len => {
                    text_units.add_unindexed(record.content, size, TextStatus::Oversize);
                }
                _ => {}
            },
            Some(OverlayEntry::Deleted) | None => {}
        }
    }
    (records, text_units)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct RefreshCounts {
    pub(crate) refreshes: u64,
    pub(crate) guard_fallbacks: u64,
    pub(crate) precheck_fallbacks: u64,
}

#[derive(Default)]
pub(super) struct RefreshWork<'a> {
    guard: Option<&'a RefreshMemory>,
    stats: UpdateStats,
}

impl fmt::Display for RefreshWork<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stats = &self.stats;
        write!(
            formatter,
            "changed={} rebound={} replayed={} resolved={} dispatch_replayed={} dispatch_recomputed={} site_files={} touched={} peak_growth_bytes=",
            stats.changed_paths, stats.rebound, stats.replayed, stats.resolved,
            stats.dispatch_replayed, stats.dispatch_recomputed, stats.sites_resolved, stats.touched.len()
        )?;
        match self.guard.and_then(RefreshMemory::peak_growth) {
            Some(bytes) => write!(formatter, "{bytes}"),
            None => formatter.write_str("unknown"),
        }
    }
}

impl Checkout {
    pub(super) fn publish_changes_with(
        &self,
        by_build: bool,
        guard: Arc<RefreshMemory>,
        started: Instant,
    ) -> Result<Option<DeltaReport>, DataError> {
        match self.try_publish_changes(by_build, guard, started) {
            Ok(report) => Ok(report),
            Err(DeltaError::Data(error)) => Err(error),
            Err(DeltaError::Memory(limit)) => self.publication_memory_fallback(by_build, limit),
        }
    }

    fn publication_memory_fallback(
        &self,
        by_build: bool,
        limit: MemoryLimit,
    ) -> Result<Option<DeltaReport>, DataError> {
        // The failed attempt has dropped its inputs and resolutions, removed
        // the spill files, and released the writer lock. Drop the engine too.
        let mut slot = self
            .engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.clear_engine_slot(&mut slot);
        self.refresh_counts.guard_fallbacks.fetch_add(1, Relaxed);
        let reservation = if by_build { None } else { self.reserve_build() };
        drop(slot);
        let stand_down = !by_build && reservation.is_none();
        crate::slog_info!(
            "call-graph memory ceiling exceeded: growth={} ceiling={} files={}; root={} guard_fallback=true stage=publication stand_down={stand_down}",
            limit.growth, limit.ceiling, limit.files, self.root.display()
        );
        if let Some(reservation) = reservation {
            self.build_reserved(false, reservation)?;
        } else if !stand_down {
            // The sibling build already holds the reservation.
            self.build_inner(false)?;
        }
        Ok(None)
    }

    pub(crate) fn refresh_counts(&self) -> RefreshCounts {
        RefreshCounts {
            refreshes: self.refresh_counts.refreshes.load(Relaxed),
            guard_fallbacks: self.refresh_counts.guard_fallbacks.load(Relaxed),
            precheck_fallbacks: self.refresh_counts.precheck_fallbacks.load(Relaxed),
        }
    }

    /// Resolves the call graph again for the paths changed since the
    /// publication, and lays the files whose resolution changed over the
    /// published graph. Returns `None` when nothing needed it.
    ///
    /// # Errors
    ///
    /// When a stored record or fragment cannot be read, or resolution
    /// fails. The pending paths stay pending then.
    pub fn refresh_callgraph(&self) -> Result<Option<UpdateStats>, DataError> {
        self.refresh_callgraph_as(false)
    }

    pub(super) fn refresh_callgraph_as(
        &self,
        by_build: bool,
    ) -> Result<Option<UpdateStats>, DataError> {
        self.refresh_callgraph_with(by_build, &RefreshMemory::start(self.family.dir()))
    }

    pub(super) fn refresh_callgraph_with(
        &self,
        by_build: bool,
        guard: &RefreshMemory,
    ) -> Result<Option<UpdateStats>, DataError> {
        let _alloc_label = crate::alloc_trace::scope(
            crate::alloc_trace::Label::CALLGRAPH_REFRESH,
            Some(&self.root),
        );
        checkpoint()?;
        let result = self.refresh_inner(by_build, guard);
        self.touch_memory();
        self.note_outcome("refresh", &result);
        result
    }

    fn refresh_inner(
        &self,
        by_build: bool,
        guard: &RefreshMemory,
    ) -> Result<Option<UpdateStats>, DataError> {
        let (published, state, changes, covered, invalidated) = {
            let live = self.live.read().expect("no panics while holding it");
            if live.building && !by_build {
                return Ok(None);
            }
            let Some(published) = live.published.clone() else {
                return Ok(None);
            };
            let state = live
                .state
                .clone()
                .unwrap_or_else(|| Arc::new(published.state.clone()));
            // What earlier refreshes since the publication resolved again,
            // each file in the class its budget admitted it to.
            let invalidated = live
                .refreshed
                .as_ref()
                .map_or_else(Default::default, |refreshed| {
                    let full = refreshed
                        .replaced
                        .keys()
                        .filter(|file| !refreshed.sites_only.contains(*file))
                        .chain(&refreshed.removed)
                        .cloned()
                        .collect::<BTreeSet<_>>();
                    let sites = refreshed
                        .sites_only
                        .iter()
                        .filter(|file| refreshed.replaced.contains_key(*file))
                        .cloned()
                        .collect::<BTreeSet<_>>();
                    (full, sites)
                });
            (
                published,
                state,
                live.changes.clone(),
                live.sequence,
                invalidated,
            )
        };
        // Without a call graph there is nothing to update; the build that
        // makes one covers every change recorded before its scan.
        if changes.is_empty() || !published.graph_built || !self.planes.callgraph {
            return Ok(None);
        }
        // An update that stopped part way through dropped its engine before
        // anything else could read the mutex, so a poisoned lock only means
        // the engine must start again.
        let mut slot = self.engine.lock().unwrap_or_else(|poisoned| {
            self.engine.clear_poison();
            let mut slot = poisoned.into_inner();
            self.clear_engine_slot(&mut slot);
            slot
        });
        if self.is_building() && !by_build {
            return Ok(None);
        }
        let parsed = slot.as_ref().map_or_else(
            || published.graph.parsed_files().count(),
            |engine| engine.live.inputs.parses.len(),
        );
        let mut budget = RefreshBudget::new(parsed, invalidated.0, invalidated.1);
        let mut work = RefreshWork {
            guard: Some(guard),
            stats: UpdateStats {
                changed_paths: changes.len(),
                ..UpdateStats::default()
            },
        };
        // A damaged stored record or fragment can stop the update where it
        // expects one (every file the engine lists has a parse and a
        // resolution). Such a stop fails this refresh as damage, and the
        // engine, whose state it may have left half-updated, starts again.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.update_engine(
                &mut slot,
                &published,
                &state,
                &changes,
                &mut budget,
                &mut work,
            )
        }));
        drop(budget);
        let (stats, served) = match outcome {
            Ok(Ok(EngineUpdate::Applied(served))) => {
                if served.is_some() {
                    crate::slog_info!(
                        "call-graph refresh: root={} guard_fallback=false {work}",
                        self.root.display()
                    );
                }
                (served.is_some().then_some(work.stats), served)
            }
            Ok(Ok(fallback @ (EngineUpdate::Cold(_) | EngineUpdate::Memory(_)))) => {
                // No incremental engine or its decoded state overlaps the cold
                // builder. Watcher events keep recording over the publication.
                self.clear_engine_slot(&mut slot);
                let reservation = if by_build { None } else { self.reserve_build() };
                drop(slot);
                drop(changes);
                drop(state);
                drop(published);
                if !by_build && reservation.is_none() {
                    return Ok(None);
                }
                match fallback {
                    EngineUpdate::Cold(limit) => {
                        self.refresh_counts.precheck_fallbacks.fetch_add(1, Relaxed);
                        crate::slog_info!(
                            "data layer cold refresh: root={} guard_fallback=false {work}; {limit}",
                            self.root.display()
                        );
                    }
                    EngineUpdate::Memory(limit) => {
                        self.refresh_counts.guard_fallbacks.fetch_add(1, Relaxed);
                        crate::slog_info!(
                            "call-graph refresh exceeded its memory ceiling: growth={} ceiling={} sites={} files={}; rebuilding cold; root={} guard_fallback=true {work}",
                            limit.growth, limit.ceiling, limit.sites, limit.files, self.root.display()
                        );
                    }
                    EngineUpdate::Applied(..) => {
                        unreachable!("only cold fallbacks reach this branch")
                    }
                }
                let started = Instant::now();
                let rebuilt = if let Some(reservation) = reservation {
                    self.build_reserved(false, reservation)?
                } else {
                    // The outer build is reusing another checkout's publication
                    // and already holds the reservation.
                    self.build_inner(false)?
                };
                return Ok(Some(UpdateStats {
                    cold_build: true,
                    cold_reused_records: rebuilt.stored_records,
                    total_ms: elapsed_ms(started),
                    ..UpdateStats::default()
                }));
            }
            Ok(Err(error)) => {
                self.clear_engine_slot(&mut slot);
                crate::slog_info!(
                    "call-graph refresh stopped: root={} guard_fallback=false {work}",
                    self.root.display()
                );
                return Err(error);
            }
            Err(panic) => {
                self.clear_engine_slot(&mut slot);
                crate::slog_info!(
                    "call-graph refresh stopped: root={} guard_fallback=false {work}",
                    self.root.display()
                );
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "no message".to_string());
                let failures = published.graph.failures();
                return Err(DataError::Damaged(if failures.is_empty() {
                    format!("the call-graph update stopped: {message}")
                } else {
                    failures.join("; ")
                }));
            }
        };
        // An engine a refresh changed is kept for the next one, and counts
        // against the budget every checkout's engine shares.
        let kept = stats.is_some().then(|| {
            self.layer.engines.touch(&self.engine);
            slot.as_mut()
                .filter(|_| self.layer.engines.measure_due(&self.engine))
                .map(|engine| {
                    engine.measure_after_publication = true;
                    engine.census_bytes()
                })
        });
        drop(slot);
        if let Some(bytes) = kept {
            self.layer.engines.kept(&self.engine, bytes);
        }
        let mut live = self.write_live();
        if live
            .published
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &published))
        {
            if let Some((overlay, refreshed)) = served {
                let graph: Arc<dyn SliceSource + Send + Sync> = Arc::new(overlay);
                live.graph = Some(graph);
                live.refreshed = Some(Arc::new(refreshed));
                self.refresh_counts.refreshes.fetch_add(1, Relaxed);
            }
            let resolved = live
                .changes
                .iter()
                .filter(|(_, change)| change.sequence <= covered)
                .map(|(path, _)| path.clone())
                .collect::<BTreeSet<_>>();
            if let Some(state) = &live.state {
                let mut gaps = state.skipped_parses().clone();
                for path in &resolved {
                    gaps.remove(path);
                    if let Extracted::GrammarGap(reason) = live.changes[path].content.get() {
                        gaps.insert(path.clone(), reason.clone());
                    }
                }
                live.state = Some(Arc::new(
                    state
                        .as_ref()
                        .clone()
                        .with_skipped_parses(gaps)
                        .expect("current change gaps"),
                ));
            }
            // The refresh extracted these changes, so each one whose
            // extraction failed now counts as unparsed. The set is copied
            // only when that changes it and a view still holds it.
            let live = &mut *live;
            for path in &resolved {
                let failed = live.changes.get(path).is_some_and(Change::failed);
                if failed != live.unparsed.contains(path) {
                    let unparsed = Arc::make_mut(&mut live.unparsed);
                    if failed {
                        unparsed.insert(path.clone());
                    } else {
                        unparsed.remove(path);
                    }
                }
            }
            live.callgraph_pending = Arc::new(
                live.callgraph_pending
                    .iter()
                    .filter(|path| !resolved.contains(*path))
                    .cloned()
                    .collect(),
            );
        }
        Ok(stats)
    }

    /// Applies `changes` to the engine in `slot` (started from `published`
    /// when it is missing or was started from an older one), returning the
    /// update's statistics and, when it applied anything, the graph and
    /// resolutions to serve.
    pub(super) fn update_engine(
        &self,
        slot: &mut Option<GraphEngine>,
        published: &Arc<Published>,
        state: &Arc<CheckoutState>,
        changes: &BTreeMap<Vec<u8>, Change>,
        budget: &mut RefreshBudget,
        work: &mut RefreshWork<'_>,
    ) -> Result<EngineUpdate, DataError> {
        match self.update_engine_checked(slot, published, state, changes, budget, work) {
            Ok(update) => Ok(update),
            Err(UpdateError::Limit(limit)) => Ok(EngineUpdate::Cold(limit)),
            Err(UpdateError::Stop(RefreshStop::Memory(limit))) => Ok(EngineUpdate::Memory(limit)),
            Err(UpdateError::Stop(RefreshStop::Cancelled)) => Err(DataError::Cancelled),
            Err(UpdateError::Resolve(error)) => Err(DataError::Resolve(error.to_string())),
            Err(UpdateError::Data(error)) => Err(error),
        }
    }

    fn update_engine_checked(
        &self,
        slot: &mut Option<GraphEngine>,
        published: &Arc<Published>,
        state: &Arc<CheckoutState>,
        changes: &BTreeMap<Vec<u8>, Change>,
        budget: &mut RefreshBudget,
        work: &mut RefreshWork<'_>,
    ) -> Result<EngineUpdate, UpdateError> {
        let guard = work.guard;
        // This test needs neither extraction nor an engine start. A single
        // watcher batch can already be larger than incremental admission.
        budget
            .check_changed(changes.len())
            .map_err(UpdateError::Limit)?;
        let changed_members = changes
            .keys()
            .filter_map(|path| std::str::from_utf8(path).ok().map(str::to_owned))
            .collect::<BTreeSet<_>>();
        budget
            .include(Stage::Changed, &changed_members)
            .map_err(UpdateError::Limit)?;
        guard.boundary()?;
        self.ensure_engine(slot, published)
            .map_err(UpdateError::Data)?;
        let engine = slot.as_mut().expect("started");
        let changed = changes
            .iter()
            .filter(|(path, change)| engine.applied.get(*path) != Some(&change.sequence))
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        if !changed.is_empty() {
            guard.boundary()?;
            note_source_work(&self.root, |work| work.resolutions += 1);
            #[cfg(feature = "test-hooks")]
            refresh_probe::stage("extraction");
            // The changed files are extracted here, all at once on the
            // engine's threads, rather than each when its edit was recorded.
            let extracting = changed
                .iter()
                .filter_map(|path| changes.get(path))
                .collect::<Vec<_>>();
            engine.live.install(|| {
                use rayon::prelude::*;
                let carry = crate::alloc_trace::carry();
                extracting.par_iter().try_for_each(|change| {
                    guard.file()?;
                    let _label = carry.enter();
                    change.content.get();
                    Ok::<_, RefreshStop>(())
                })
            })?;
            guard.boundary()?;
            #[cfg(feature = "test-hooks")]
            refresh_probe::stage("planning");
            let next = next_inputs(state, &engine.live.inputs, changes, &changed, guard)?;
            budget
                .set_parsed(next.parses.len())
                .map_err(UpdateError::Limit)?;
            let changed_names = changed
                .iter()
                .filter_map(|path| std::str::from_utf8(path).ok().map(str::to_string))
                .collect::<BTreeSet<_>>();
            let update = engine.live.update_with_budget(
                next,
                &changed_names,
                Some(budget),
                guard,
                &mut work.stats,
            );
            // The engine reads stored parses and resolutions as it goes; a
            // read that failed made it work from an empty one.
            let failures = published.graph.failures();
            if !failures.is_empty() {
                return Err(UpdateError::Data(DataError::Damaged(format!(
                    "reading the stored call graph failed: {}",
                    failures.join("; ")
                ))));
            }
            // The caller drops a partly updated engine after either error,
            // so the next refresh starts again from the publication.
            update?;
            guard.boundary()?;
            #[cfg(feature = "test-hooks")]
            refresh_probe::stage("slices");
            // Only the files this update touched get new slices; every file
            // touched by an earlier update keeps the slice it got then. Each
            // is laid over the publication encoded, as a publication stores
            // it, and the refreshed files keep each resolution encoded, so
            // what an edit resolved again costs its encoded size until the
            // next publication. The slices and encodings are made on the
            // engine's threads.
            let cached_as = CacheKey::new();
            let files = work.stats.touched.iter().collect::<Vec<_>>();
            let (resolved, parses) = (&engine.live.resolved, &engine.live.inputs.parses);
            let graph = &published.graph;
            let made = engine.live.install(|| {
                use rayon::prelude::*;
                let carry = crate::alloc_trace::carry();
                files
                    .par_iter()
                    .map(|file| {
                        guard.file()?;
                        let _label = carry.enter();
                        let (Some(resolution), Some((key, parse))) =
                            (resolved.get(file), parses.get(file))
                        else {
                            return Ok(None);
                        };
                        let slice = FileSlice::build_checked(file, key, parse, resolution, guard)?;
                        let Some(resolution) = resolved.encoded(file, key) else {
                            return Ok(None);
                        };
                        let refreshed = RefreshedFile {
                            resolution,
                            parse: (graph.record_key_of(file) != Some(key.as_str()))
                                .then(|| Arc::clone(parse)),
                        };
                        Ok(Some((
                            PreparedSlice::of(&slice, key, &cached_as),
                            refreshed,
                        )))
                    })
                    .collect::<Result<Vec<_>, RefreshStop>>()
            })?;
            for (file, made) in files.into_iter().zip(made) {
                guard.file()?;
                // The next refresh's budget counts this file in the class
                // this one admitted it to. A file any refresh since the
                // publication resolved in full stays counted as full work.
                if made.is_some() && !budget.full_work(file) {
                    if !engine.refreshed.replaced.contains_key(file) {
                        engine.refreshed.sites_only.insert(file.clone());
                    }
                } else {
                    engine.refreshed.sites_only.remove(file);
                }
                match made {
                    Some((slice, refreshed)) => {
                        engine.overlay.replace_encoded(file, slice);
                        engine.refreshed.removed.remove(file);
                        engine.refreshed.replaced.insert(file.clone(), refreshed);
                    }
                    None => {
                        engine.overlay.remove(file);
                        engine.refreshed.replaced.remove(file);
                        engine.refreshed.removed.insert(file.clone());
                    }
                }
            }
            engine.refreshed.state = engine.live.inputs.state.clone();
            for path in &changed {
                guard.file()?;
                engine.applied.insert(path.clone(), changes[path].sequence);
                let Ok(name) = std::str::from_utf8(path) else {
                    continue;
                };
                match changes[path].content.get() {
                    Extracted::Parse(_, _, record) => {
                        engine
                            .refreshed
                            .records
                            .insert(name.to_string(), Arc::clone(record));
                    }
                    Extracted::Config(..)
                    | Extracted::Markers(_)
                    | Extracted::GrammarGap(_)
                    | Extracted::ConversionFailed
                    | Extracted::Nothing => {
                        engine.refreshed.records.remove(name);
                    }
                }
            }
            guard.boundary()?;
            note_source_work(&self.root, |work| {
                let live = &engine.live;
                work.decoded_parses = work.decoded_parses.max(live.inputs.parses.decoded_count());
                work.decoded_resolutions =
                    work.decoded_resolutions.max(live.resolved.decoded_count());
            });
            // An update keeps every parse and resolution it decoded. A broad
            // one reads thousands (every file declaring a method named `new`,
            // say), and resolves a thousand again after a widely read change,
            // so past a budget every one that can be decoded again goes: from
            // the publication, or for a resolution an update made, from the
            // encoding it got above. A later update decodes again what it
            // reads. A changed file's parse stays, since only the changes
            // hold it until the next publication.
            if engine.live.inputs.parses.decoded_count() > ENGINE_DECODED_FILES {
                engine.live.inputs.parses.release_decoded();
            }
            if engine.live.resolved.decoded_count() > ENGINE_DECODED_FILES {
                engine.live.resolved.release_decoded();
            }
        }
        // A refresh that applied nothing keeps the served graph, and with it
        // any dead-code snapshot already built for it.
        let served =
            (!changed.is_empty()).then(|| (engine.overlay.clone(), engine.refreshed.clone()));
        guard.boundary()?;
        Ok(EngineUpdate::Applied(served))
    }
}
