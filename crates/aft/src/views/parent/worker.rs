//! The parent session worker: finds the children, loads each child plane
//! once, follows the children's publications, and applies file changes.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};

use crate::readonly_artifacts::ReadOnlyArtifact;

use super::super::contracts::{PlaneAdapter, ViewAccess};
use super::super::manifest_v2::Producers;
use super::super::semantic::SemanticProducer;
use super::super::snapshot::LiveDelta;
use super::{
    lock, read, write, CallgraphChild, Child, Discovery, ParentSemantic, ParentSession, Plane,
    PublishedChild, SemanticChild, TrigramChild, TrigramSource, DEFAULT_MAX_CHILD_REPOS,
};

/// Longest the worker sleeps between two checks of its stop flag, holders
/// and pending paths when no watcher event wakes it.
const TICK: Duration = Duration::from_millis(250);

/// Threads that load the children's trigram snapshots when a session starts.
const INITIAL_LOAD_THREADS: usize = 4;

/// Directory names a child's own trigram walk never enters; a new file under
/// one is not added to the snapshot either.
const UNINDEXED_DIRECTORY_NAMES: &[&str] = &[
    "node_modules",
    "target",
    "venv",
    ".venv",
    ".git",
    "__pycache__",
    ".tox",
    "dist",
    "build",
];

pub(super) fn run(weak: Weak<ParentSession>) {
    let Some(first) = weak.upgrade() else {
        return;
    };
    if !discover(&first) {
        return;
    }
    initial_load(&first);
    first.rounds.fetch_add(1, Ordering::SeqCst);
    crate::slog_info!(
        "parent folder loaded root={} children={}",
        first.root().display(),
        first.children().len()
    );
    let mut next_pointer_check = Instant::now() + first.pointer_interval;
    let mut next_backstop = Instant::now() + first.backstop_interval;
    drop(first);
    loop {
        let Some(session) = weak.upgrade() else {
            return;
        };
        if session.stopped() {
            return;
        }
        if !session.held() {
            crate::slog_info!(
                "parent folder released root={} (no bound context holds it)",
                session.root().display()
            );
            session.stop();
            super::unregister(&session);
            return;
        }
        let paused = session.paused.load(Ordering::SeqCst);
        if !paused {
            for child in session.children() {
                apply_pending(&child);
            }
            let now = Instant::now();
            if now >= next_backstop {
                for child in session.children() {
                    if session.stopped() {
                        return;
                    }
                    backstop(&child);
                }
                next_backstop = Instant::now() + session.backstop_interval;
            }
            if now >= next_pointer_check {
                refresh_publications(&session);
                session.rounds.fetch_add(1, Ordering::SeqCst);
                next_pointer_check = Instant::now() + session.pointer_interval;
            }
        }
        let wait = if paused {
            Duration::from_millis(20)
        } else {
            TICK.min(next_pointer_check.saturating_duration_since(Instant::now()))
        };
        // Hold no strong reference while waiting, so a dropped session ends.
        drop(session);
        if let Some(session) = weak.upgrade() {
            session.wait_for_wake(wait);
        }
    }
}

/// The full discovery walk. False when the session ended meanwhile.
fn discover(session: &Arc<ParentSession>) -> bool {
    super::wait_for_discovery_hold(session.root());
    let started = Instant::now();
    let discovery = super::discover(session.root(), DEFAULT_MAX_CHILD_REPOS).unwrap_or(Discovery {
        root: session.root().to_path_buf(),
        children: Vec::new(),
        skipped: Vec::new(),
        outside: Vec::new(),
    });
    if session.stopped() {
        return false;
    }
    let children = discovery
        .children
        .iter()
        .map(|child| Arc::new(Child::new(session.root(), child.clone())))
        .collect::<Vec<_>>();
    for child in &children {
        if !session.planes.trigram {
            *write(&child.trigram) = Plane::Gap("the trigram index is off".into());
        }
        if !session.planes.callgraph {
            *write(&child.callgraph) = Plane::Gap("the call graph is off".into());
        }
        if !session.planes.semantic {
            *write(&child.semantic) = Plane::Gap("semantic search is off".into());
        }
    }
    *write(&session.children) = children;
    *write(&session.discovery) = Some(Arc::new(discovery));
    crate::slog_info!(
        "parent folder discovered root={} children={} elapsed_ms={}",
        session.root().display(),
        session.children().len(),
        started.elapsed().as_millis()
    );
    true
}

fn initial_load(session: &ParentSession) {
    let children = session.children();
    if session.planes.trigram && !children.is_empty() {
        // A few children load at once: reading an artifact and reconciling
        // it with the child's files is mostly I/O, and a folder of forty
        // repositories should not wait for them one after another.
        let per_thread = children.len().div_ceil(INITIAL_LOAD_THREADS);
        std::thread::scope(|scope| {
            for chunk in children.chunks(per_thread) {
                scope.spawn(move || {
                    for child in chunk {
                        if session.stopped() {
                            return;
                        }
                        refresh_trigram(session, child);
                    }
                });
            }
        });
    }
    for child in &children {
        if session.stopped() {
            return;
        }
        if session.planes.callgraph {
            refresh_callgraph(session, child);
        }
        refresh_inspect(session, child);
    }
    if session.planes.semantic {
        start_semantic(session);
        for child in &children {
            if session.stopped() {
                return;
            }
            refresh_semantic(session, child);
        }
    }
}

/// Follows each child's publications: a rewritten trigram artifact, a new
/// view generation, a first inspect run.
fn refresh_publications(session: &ParentSession) {
    for child in session.children() {
        if session.stopped() {
            return;
        }
        if session.planes.trigram {
            refresh_trigram(session, &child);
        }
        if session.planes.callgraph {
            refresh_callgraph(session, &child);
        }
        refresh_inspect(session, &child);
        if session.planes.semantic {
            refresh_semantic(session, &child);
        }
    }
}

// ---------------------------------------------------------------------------
// Trigram: load once, then apply changed paths
// ---------------------------------------------------------------------------

/// Installs a new trigram snapshot for `child` and hands it to the child's
/// search engine.
fn install_trigram(session: &ParentSession, child: &Child, trigram: TrigramChild) {
    let trigram = Arc::new(trigram);
    *write(&child.trigram) = Plane::Ready(Arc::clone(&trigram));
    super::engine::sync(session, child);
}

fn refresh_trigram(session: &ParentSession, child: &Child) {
    let keep_going = || !session.stopped();
    let family = match child.family(&session.storage) {
        Ok(family) => family,
        Err(error) => {
            *write(&child.trigram) = Plane::Gap(error);
            return;
        }
    };
    // A child whose sessions run with views on publishes its lexical index
    // only as its v3 lexical view; the parent reads that publication.
    match open_child_lexical_view(session, child, family) {
        Ok(None) => {}
        Ok(Some(generation)) => {
            install_view_trigram(session, child, family, &generation);
            return;
        }
        Err(error) => {
            *write(&child.trigram) = Plane::Gap(format!("lexical view unavailable: {error}"));
            return;
        }
    }
    let artifact = crate::readonly_artifacts::search_index_artifact_generation_with_key(
        family,
        Some(&session.storage),
    );
    let Some(artifact) = artifact else {
        let mut trigram = write(&child.trigram);
        // Releasing the ledger lock does not itself publish a usable lexical index.
        // Keep its last busy diagnostic until a usable publication arrives.
        if matches!(&*trigram, Plane::Gap(reason) if super::super::trigram::bind_is_busy(reason)) {
            return;
        }
        *trigram = Plane::Gap("no trigram index has been built for this repository yet".into());
        return;
    };
    if read(&child.trigram).ready().is_some_and(
        |current| matches!(&current.source, TrigramSource::Legacy(current) if *current == artifact),
    ) {
        // Same artifact: changes since it was read arrive as watcher paths
        // and through the backstop, never by reading the artifact again.
        return;
    }
    let cache_dir = crate::search_index::resolve_cache_dir_with_key(family, Some(&session.storage));
    child.loads.fetch_add(1, Ordering::SeqCst);
    let opened = crate::readonly_artifacts::open_search_index_background(
        &child.root,
        cache_dir,
        &keep_going,
    );
    let mut index = match opened {
        ReadOnlyArtifact::Fresh(index) => index,
        ReadOnlyArtifact::Stale(stale) => stale.index,
        ReadOnlyArtifact::Degraded(degradation) => {
            *write(&child.trigram) = Plane::Gap(format!(
                "the trigram index could not be read ({})",
                degradation.reason
            ));
            return;
        }
        ReadOnlyArtifact::Absent => {
            *write(&child.trigram) =
                Plane::Gap("no readable trigram index for this repository".into());
            return;
        }
        ReadOnlyArtifact::Cancelled => return,
    };
    // The saved artifact describes the child as its own session last wrote
    // it. Edits made since then are applied to this in-memory copy only; the
    // child's artifact is never written.
    if index
        .reconcile_borrowed_snapshot_with_disk(&keep_going)
        .is_none()
    {
        return;
    }
    // Paths queued before this load are already reflected by the reconcile.
    lock(&child.pending).clear();
    install_trigram(
        session,
        child,
        TrigramChild {
            index: Arc::new(index),
            source: TrigramSource::Legacy(artifact),
        },
    );
}

/// The child's published v3 lexical view, or `None` when none is published
/// yet (a child bound only with views off, never bound, or not yet bound).
fn open_child_lexical_view(
    session: &ParentSession,
    child: &Child,
    family: &str,
) -> Result<Option<Arc<super::super::snapshot::ComposedGeneration>>, String> {
    let producers = Producers {
        trigram: session.trigram_policy.fingerprint_hex(),
        semantic: None,
        callgraph: super::super::semantic_runtime::UNREGISTERED_PRODUCER.into(),
    };
    super::super::base_refs::open_foreign_v3(&session.storage, family, &child.scope, &producers)
        .map_err(|error| error.to_string())
}

/// Installs the trigram snapshot of a child's published v3 lexical view
/// unless the parent already serves that generation.
fn install_view_trigram(
    session: &ParentSession,
    child: &Child,
    family: &str,
    generation: &Arc<super::super::snapshot::ComposedGeneration>,
) {
    let published = PublishedChild::of(generation);
    if read(&child.trigram).ready().is_some_and(
        |current| matches!(&current.source, TrigramSource::View(current) if *current == published),
    ) {
        return;
    }
    child.loads.fetch_add(1, Ordering::SeqCst);
    match super::super::trigram::project_published(
        &child.root,
        &session.storage,
        family,
        session.trigram_policy,
        generation,
    ) {
        Ok(snapshot) => {
            // Changes made since the child published arrive with the child's
            // next publication; the parent never applies them itself.
            lock(&child.pending).clear();
            install_trigram(
                session,
                child,
                TrigramChild {
                    index: Arc::new(crate::search_index::SearchIndex::from_view_snapshot(
                        snapshot,
                    )),
                    source: TrigramSource::View(published),
                },
            );
        }
        Err(error) => {
            *write(&child.trigram) = Plane::Gap(format!("lexical view unreadable: {error}"));
        }
    }
}

/// Applies the watcher paths queued for `child`.
fn apply_pending(child: &Arc<Child>) {
    let paths = std::mem::take(&mut *lock(&child.pending));
    if paths.is_empty() {
        return;
    }
    if paths.iter().any(|path| {
        path.file_name()
            .is_some_and(|name| name == ".gitignore" || name == ".aftignore")
    }) {
        lock(&child.ignore_files).clear();
    }
    apply_paths(child, paths);
}

/// Re-reads exactly `paths` into a copy of the child's snapshot and installs
/// it. Indexed paths are updated or dropped; a new file is added unless the
/// child's ignore rules exclude it. Queries keep the previous snapshot until
/// the copy is installed.
fn apply_paths(child: &Arc<Child>, paths: BTreeSet<PathBuf>) {
    let Some(current) = read(&child.trigram).ready().cloned() else {
        // Not loaded yet: the first load reconciles the whole child anyway.
        return;
    };
    if matches!(current.source, TrigramSource::View(_)) {
        return;
    }
    let mut index = (*current.index).clone();
    let mut applied = 0usize;
    for path in &paths {
        let indexed = index.path_to_id.contains_key(path)
            || std::fs::canonicalize(path)
                .ok()
                .is_some_and(|canonical| index.path_to_id.contains_key(&canonical));
        if indexed {
            index.update_file(path);
            applied += 1;
        } else if path.is_file() && belongs_to_child(child, path) {
            index.update_file(path);
            applied += 1;
        }
    }
    if applied == 0 {
        return;
    }
    child.applied.fetch_add(applied, Ordering::SeqCst);
    let session_less = TrigramChild {
        index: Arc::new(index),
        source: current.source.clone(),
    };
    *write(&child.trigram) = Plane::Ready(Arc::new(session_less));
    super::engine::sync_trigram(child);
}

/// Compares every file of `child` with its trigram snapshot and applies the
/// differences. It walks and stats only; the snapshot is copied only when a
/// difference exists.
fn backstop(child: &Arc<Child>) {
    let Some(current) = read(&child.trigram).ready().cloned() else {
        return;
    };
    if matches!(current.source, TrigramSource::View(_)) {
        return;
    }
    let walked = crate::search_index::walk_project_files(
        &child.root,
        &crate::search_index::PathFilters::default(),
    );
    let walked_set = walked.iter().collect::<std::collections::HashSet<_>>();
    let mut changed = BTreeSet::new();
    for entry in current.index.files.iter() {
        if entry.path.as_os_str().is_empty() {
            continue;
        }
        if !walked_set.contains(&entry.path) {
            changed.insert(entry.path.clone());
            continue;
        }
        let differs = std::fs::metadata(&entry.path).map_or(true, |metadata| {
            metadata.len() != entry.size
                || metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH) != entry.modified
        });
        if differs {
            changed.insert(entry.path.clone());
        }
    }
    for path in walked {
        if !current.index.path_to_id.contains_key(&path) {
            changed.insert(path);
        }
    }
    if !changed.is_empty() {
        // The walk already applied the child's ignore rules to new files.
        apply_walked_paths(child, changed);
    }
}

fn apply_walked_paths(child: &Arc<Child>, paths: BTreeSet<PathBuf>) {
    let Some(current) = read(&child.trigram).ready().cloned() else {
        return;
    };
    let mut index = (*current.index).clone();
    for path in &paths {
        index.update_file(path);
    }
    child.applied.fetch_add(paths.len(), Ordering::SeqCst);
    *write(&child.trigram) = Plane::Ready(Arc::new(TrigramChild {
        index: Arc::new(index),
        source: current.source.clone(),
    }));
    super::engine::sync_trigram(child);
}

/// Ignore files read so far for one child, with the mtime each was read at.
#[derive(Default)]
pub(crate) struct IgnoreCache {
    files: HashMap<PathBuf, Option<ignore::gitignore::Gitignore>>,
}

impl IgnoreCache {
    pub(crate) fn clear(&mut self) {
        self.files.clear();
    }

    fn matcher(&mut self, path: PathBuf, base: &Path) -> Option<&ignore::gitignore::Gitignore> {
        self.files
            .entry(path.clone())
            .or_insert_with(|| {
                if !path.is_file() {
                    return None;
                }
                let mut builder = ignore::gitignore::GitignoreBuilder::new(base);
                builder.add(&path);
                builder.build().ok()
            })
            .as_ref()
    }
}

/// Whether a file the child's snapshot does not hold yet belongs in it: not
/// under a directory the child's walk skips and not excluded by the ignore
/// files between the child root and the file. The deepest ignore file that
/// decides wins, as in git.
fn belongs_to_child(child: &Child, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(&child.root) else {
        return false;
    };
    if relative.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| UNINDEXED_DIRECTORY_NAMES.contains(&name))
    }) {
        return false;
    }
    if crate::project_content::is_excluded(path) {
        return false;
    }
    let mut directories = path
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(&child.root))
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    // Deepest directory first, then the repository's own exclude file.
    let mut cache = lock(&child.ignore_files);
    for dir in directories.drain(..) {
        for name in [".aftignore", ".gitignore"] {
            if let Some(matcher) = cache.matcher(dir.join(name), &dir) {
                let decision = matcher.matched_path_or_any_parents(path, false);
                if decision.is_ignore() {
                    return false;
                }
                if decision.is_whitelist() {
                    return true;
                }
            }
        }
    }
    let exclude = child.root.join(".git").join("info").join("exclude");
    if let Some(matcher) = cache.matcher(exclude, &child.root) {
        if matcher.matched_path_or_any_parents(path, false).is_ignore() {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Call graph, inspect and semantic publications
// ---------------------------------------------------------------------------

/// Follows the child's published v3 call-graph view. The parent answers as of
/// that publication and holds its read marker; a child whose sessions never
/// published one is a named gap.
fn refresh_callgraph(session: &ParentSession, child: &Child) {
    let family = match child.family(&session.storage) {
        Ok(family) => family,
        Err(error) => {
            *write(&child.callgraph) = Plane::Gap(error);
            return;
        }
    };
    let producers = Producers {
        trigram: session.trigram_policy.fingerprint_hex(),
        semantic: None,
        callgraph: super::super::callgraph::PRODUCER.into(),
    };
    let scope = super::super::callgraph::view_scope(&child.root);
    let generation = match super::super::base_refs::open_foreign_v3(
        &session.storage,
        family,
        &scope,
        &producers,
    ) {
        Ok(Some(generation)) => generation,
        Ok(None) => {
            *write(&child.callgraph) =
                Plane::Gap("no call graph view has been published for this repository yet".into());
            return;
        }
        Err(error) => {
            *write(&child.callgraph) = Plane::Gap(format!("call graph view unavailable: {error}"));
            return;
        }
    };
    let published = PublishedChild::of(&generation);
    if read(&child.callgraph)
        .ready()
        .is_some_and(|current| current.published == published)
    {
        return;
    }
    let Some(route) = generation.v3_route().cloned() else {
        *write(&child.callgraph) = Plane::Gap("call graph view has no v3 route".into());
        return;
    };
    match crate::callgraph_store::ReadonlyCallGraphStore::open_pinned_derived(
        child.root.clone(),
        family.to_owned(),
        route.member_dir(),
        generation.name(),
    ) {
        Ok(store) => {
            child.loads.fetch_add(1, Ordering::SeqCst);
            // The store keeps the generation, and with it the generation's
            // read marker, for as long as the parent serves it.
            *write(&child.callgraph) = Plane::Ready(Arc::new(CallgraphChild {
                store: Arc::new(store.retain_generation(generation)),
                published,
            }));
        }
        Err(error) => {
            *write(&child.callgraph) = Plane::Gap(match error {
                crate::callgraph_store::CallGraphStoreError::Unavailable(reason) => reason,
                other => format!("call graph view unreadable: {other}"),
            });
        }
    }
}

fn refresh_inspect(session: &ParentSession, child: &Child) {
    if read(&child.inspect).is_some() {
        return;
    }
    // Opening read-only creates nothing in the child's inspect directory but
    // a read marker; `None` means the child's sessions never stored
    // project-wide inspect results.
    if let Ok(Some(cache)) = crate::inspect::cache::InspectCache::open_readonly(
        session.storage.join("inspect"),
        child.root.clone(),
    ) {
        child.loads.fetch_add(1, Ordering::SeqCst);
        *write(&child.inspect) = Some(Arc::new(cache));
    }
}

fn start_semantic(session: &ParentSession) {
    let config = &session.semantic_config;
    let started = (|| -> Result<ParentSemantic, String> {
        let mut model = crate::semantic_index::EmbeddingModel::from_config(config)?;
        let fingerprint = model.fingerprint(config)?;
        let producer =
            SemanticProducer::current(fingerprint.as_string(), fingerprint.embed_text_caps);
        let producer_id = producer.id();
        let plane = super::super::semantic_runtime::shared_plane(&session.storage, producer);
        Ok(ParentSemantic {
            model: std::sync::Mutex::new(Some(model)),
            plane,
            producer_id,
        })
    })();
    *write(&session.semantic) = match started {
        Ok(semantic) => Plane::Ready(Arc::new(semantic)),
        Err(error) => Plane::Gap(format!("embedding model unavailable: {error}")),
    };
}

fn refresh_semantic(session: &ParentSession, child: &Child) {
    let semantic = match &*read(&session.semantic) {
        Plane::Ready(semantic) => Arc::clone(semantic),
        Plane::Loading => return,
        Plane::Gap(reason) => {
            *write(&child.semantic) = Plane::Gap(reason.clone());
            return;
        }
    };
    // The child's semantic view is its v3 member `<scope>.semantic`. Opening
    // it pins the member's published generation and the base it composes
    // with read markers; the answers are as of that publication.
    let family = match child.family(&session.storage) {
        Ok(family) => family,
        Err(error) => {
            *write(&child.semantic) = Plane::Gap(error);
            return;
        }
    };
    let scope = super::super::semantic_runtime::view_scope(&child.scope);
    let producers = Producers {
        trigram: session.trigram_policy.fingerprint_hex(),
        semantic: Some(semantic.producer_id.clone()),
        callgraph: super::super::callgraph::PRODUCER.into(),
    };
    let generation = match super::super::base_refs::open_foreign_v3(
        &session.storage,
        family,
        &scope,
        &producers,
    ) {
        Ok(Some(generation)) => generation,
        Ok(None) => {
            *write(&child.semantic) =
                Plane::Gap("no semantic view has been published for this repository yet".into());
            return;
        }
        Err(error) => {
            *write(&child.semantic) = Plane::Gap(format!("semantic view unavailable: {error}"));
            return;
        }
    };
    // The same worker generation composed with the same base generation: the
    // parent already serves it. (A base readiness successor keeps the worker's
    // name but changes what it composes.)
    if read(&child.semantic)
        .ready()
        .is_some_and(|current| current.generation.resident_key() == generation.resident_key())
    {
        return;
    }
    let reader = match super::super::registry::ReaderRegistration::for_v3_foreign_read(
        &generation,
        "parent-folder",
    ) {
        Ok(reader) => Arc::new(reader),
        Err(error) => {
            *write(&child.semantic) = Plane::Gap(format!("semantic view unreadable: {error}"));
            return;
        }
    };
    let access = ViewAccess::Reader {
        registration: reader,
        scope,
    };
    child.loads.fetch_add(1, Ordering::SeqCst);
    // Admission decodes the generation's vectors once into the family's
    // shared vector arena. There is no size cap: a large child is served like
    // a small one.
    if let Err(error) = semantic.plane.open_generation(&access, &generation) {
        *write(&child.semantic) = Plane::Gap(format!("semantic view unavailable: {error}"));
        return;
    }
    let snapshot = LiveDelta::new(Arc::clone(&generation)).snapshot();
    let previous = std::mem::replace(
        &mut *write(&child.semantic),
        Plane::Ready(Arc::new(SemanticChild {
            access: access.clone(),
            generation,
            snapshot,
        })),
    );
    super::engine::sync(session, child);
    if let Plane::Ready(previous) = previous {
        // Free the replaced generation's admitted vectors unless a session of
        // the child in this same process still serves them.
        if super::super::semantic_runtime::live_holders(&semantic.plane, &access) == 0 {
            semantic
                .plane
                .release_generation(&access, previous.generation.name());
        }
    }
}
