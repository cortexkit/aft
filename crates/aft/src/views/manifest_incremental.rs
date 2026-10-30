use super::*;
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[cfg(test)]
thread_local! {
    static PATH_COMPARISONS: std::cell::Cell<(usize, Option<usize>)> = const { std::cell::Cell::new((0, None)) };
    static PRECHECK_PATH_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[inline]
fn record_path_comparison() {
    #[cfg(test)]
    PATH_COMPARISONS.with(|stats| {
        let (count, limit) = stats.get();
        let count = count + 1;
        stats.set((count, limit));
        assert!(
            limit.is_none_or(|limit| count <= limit),
            "watcher delta comparison budget exceeded: {count}"
        );
    });
}

fn compare_paths(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
    record_path_comparison();
    left.cmp(right)
}

#[derive(Default)]
/// Byte-sorted sets shared with the HEAD listing. Each changed path needs at
/// most two logarithmic lookups, even for a checkout-wide watcher batch.
pub(super) struct HeadPaths {
    members: Vec<Vec<u8>>,
    directories: Vec<Vec<u8>>,
}

impl HeadPaths {
    fn new(head: &[crate::alias::TrackedPath]) -> Self {
        let mut paths = Self::default();
        for entry in head {
            paths.members.push(entry.rel_path.clone());
            for (index, byte) in entry.rel_path.iter().enumerate() {
                if *byte == b'/' {
                    paths.directories.push(entry.rel_path[..index].to_vec());
                }
            }
        }
        for values in [&mut paths.members, &mut paths.directories] {
            values.sort_unstable_by(|left, right| compare_paths(left, right));
            values.dedup_by(|left, right| compare_paths(left, right).is_eq());
            values.shrink_to_fit();
        }
        paths
    }

    fn contains(values: &[Vec<u8>], path: &[u8]) -> bool {
        values
            .binary_search_by(|value| compare_paths(value, path))
            .is_ok()
    }
}

/// The HEAD directories whose members a watcher batch obliges a publication to
/// read again, or `None` when the whole checkout must be read.
///
/// A named member is read again by name. A named HEAD directory may be the only
/// evidence that its children changed, were removed or were renamed away, so
/// every member under it is read again. Every other member keeps its previous
/// entry. A named path that is neither (a new untracked file, an editor's
/// temporary file, a rename destination) has no member under it, because every
/// ancestor of a member is a HEAD directory; `nonmember_is_inert` decides
/// whether such a path can still stand for a member under another spelling.
/// A batch that names no member and no HEAD directory at all is not evidence
/// that the watcher observed the checkout, so it still reads everything.
pub(super) fn delta_read_scope(
    head: &[crate::alias::TrackedPath],
    paths: &OnceLock<HeadPaths>,
    changed: &BTreeSet<Vec<u8>>,
    mut nonmember_is_inert: impl FnMut(&[u8]) -> bool,
) -> Option<BTreeSet<Vec<u8>>> {
    let paths = paths.get_or_init(|| HeadPaths::new(head));
    let mut directories = BTreeSet::new();
    let mut names_head_path = false;
    for path in changed {
        if HeadPaths::contains(&paths.directories, path) {
            directories.insert(path.clone());
            names_head_path = true;
        } else if HeadPaths::contains(&paths.members, path) {
            names_head_path = true;
        } else if !nonmember_is_inert(path) {
            return None;
        }
    }
    names_head_path.then_some(directories)
}

/// Whether `path` lies under one of `directories` (HEAD directories from
/// [`delta_read_scope`]).
pub(super) fn within_directories(directories: &BTreeSet<Vec<u8>>, path: &[u8]) -> bool {
    !directories.is_empty()
        && path
            .iter()
            .enumerate()
            .any(|(index, byte)| *byte == b'/' && directories.contains(&path[..index]))
}

/// [`delta_read_scope`] for a checkout publication. A named path that is no
/// HEAD member and no HEAD directory forces a full read when it may be a member
/// under another name: a spelling that a case-insensitive or Unicode-normalizing
/// file system, or Windows, can resolve to a member; an ignore or attributes
/// file; a symlink, or a path reached through a symlinked directory; or a file
/// with another hard link.
pub(super) fn checkout_delta_scope(
    request: &AssemblyRequest,
    head: &[crate::alias::TrackedPath],
    paths: &OnceLock<HeadPaths>,
    snapshot: Option<&HeadSnapshot>,
) -> Option<BTreeSet<Vec<u8>>> {
    let relevant = snapshot.and_then(relevant_names);
    let mut canonical_root = None;
    delta_read_scope(head, paths, &request.changed_paths, |path| {
        let Some(relevant) = relevant else {
            return false;
        };
        if uncertain_spelling(path)
            || relevant.contains(&path.to_ascii_lowercase())
            || is_rule_file(path)
        {
            return false;
        }
        canonical_root
            .get_or_insert_with(|| fs::canonicalize(&request.project_root).ok())
            .as_deref()
            .is_some_and(|canonical| {
                nonmember_on_disk_is_inert(&request.project_root, canonical, path)
            })
    })
}

/// Spellings that some file system may resolve to a differently spelled path:
/// non-ASCII names (Unicode normalization), Windows separators, drive and
/// stream colons, short-name tildes, and trailing dots or spaces.
fn uncertain_spelling(path: &[u8]) -> bool {
    !path.is_ascii()
        || RelPath::new(path.to_vec()).is_err()
        || path.iter().any(|byte| matches!(byte, b'\\' | b':' | b'~'))
        || path
            .split(|byte| *byte == b'/')
            .any(|part| part.last().is_some_and(|byte| matches!(byte, b'.' | b' ')))
}

fn is_rule_file(path: &[u8]) -> bool {
    matches!(
        path.to_ascii_lowercase()
            .rsplit(|byte| *byte == b'/')
            .next(),
        Some(b".gitattributes" | b".gitignore" | b".aftignore" | b".ignore")
    )
}

/// Every HEAD member and HEAD directory, ASCII-lowercased, or `None` when a
/// member name is not ASCII and cannot be folded reliably. Built once per HEAD.
fn relevant_names(head: &HeadSnapshot) -> Option<&BTreeSet<Vec<u8>>> {
    head.relevant
        .get_or_init(|| {
            #[cfg(test)]
            PRECHECK_PATH_BUILDS.with(|builds| builds.set(builds.get() + 1));
            let mut relevant = BTreeSet::new();
            for entry in head.entries.iter() {
                if !entry.rel_path.is_ascii() {
                    return None;
                }
                let path = entry.rel_path.to_ascii_lowercase();
                for (index, byte) in path.iter().enumerate() {
                    if *byte == b'/' {
                        relevant.insert(path[..index].to_vec());
                    }
                }
                relevant.insert(path);
            }
            Some(relevant)
        })
        .as_ref()
}

/// The named path, or its nearest existing ancestor when it is gone, must be a
/// plain file or directory that the checkout reaches without a symlink, and a
/// file must have no other hard link. Anything else may be a member under
/// another name. `path` is ASCII (see [`uncertain_spelling`]).
fn nonmember_on_disk_is_inert(root: &Path, canonical_root: &Path, path: &[u8]) -> bool {
    let parts = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
    for depth in (0..=parts.len()).rev() {
        let mut absolute = root.to_path_buf();
        let mut expected = canonical_root.to_path_buf();
        for part in &parts[..depth] {
            let part = String::from_utf8_lossy(part);
            absolute.push(part.as_ref());
            expected.push(part.as_ref());
        }
        let metadata = match fs::symlink_metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return false,
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
        if depth == parts.len()
            && !metadata.is_dir()
            && !matches!(
                crate::file_identity::metadata_with_link_count(&absolute),
                Ok((_, crate::file_identity::FileIdentity::Known { .. }, 1))
            )
        {
            return false;
        }
        return fs::canonicalize(&absolute).is_ok_and(|canonical| canonical == expected);
    }
    false
}

type RemovalStamp = (Vec<u8>, i64);

#[derive(Clone, PartialEq, Eq)]
pub(super) struct StoreStamps(RemovalStamp, RemovalStamp);

impl StoreStamps {
    pub(super) fn read(callgraph: &BlobStore, semantic: &BlobStore) -> Result<Self> {
        Ok(Self(
            callgraph.removal_stamp().map_err(store_error)?,
            semantic.removal_stamp().map_err(store_error)?,
        ))
    }
}

fn store_error(error: crate::blob_store::BlobStoreError) -> ViewError {
    ViewError::InvalidManifest(error.to_string())
}

#[derive(Clone)]
pub(super) struct CachedManifest {
    pub generation: String,
    pub manifest: Manifest,
    pub stamps: StoreStamps,
    pub tree: String,
    pub metadata: Arc<BTreeMap<Vec<u8>, fs::Metadata>>,
    pub admission: crate::db::file_identity::AdmissionSnapshot,
    pub head: Option<HeadSnapshot>,
}

#[derive(Clone)]
pub(super) struct HeadSnapshot {
    tree: String,
    index: IndexStamp,
    entries: Arc<Vec<crate::alias::TrackedPath>>,
    paths: Arc<OnceLock<HeadPaths>>,
    relevant: Arc<OnceLock<Option<BTreeSet<Vec<u8>>>>>,
}

pub(super) struct HeadRead {
    pub tree: String,
    pub entries: Arc<Vec<crate::alias::TrackedPath>>,
    pub cached: bool,
    pub snapshot: Option<HeadSnapshot>,
    pub paths: Arc<OnceLock<HeadPaths>>,
}

struct RootCache {
    previous: Option<CachedManifest>,
    head: Option<HeadSnapshot>,
    verification: Arc<Mutex<VerificationState>>,
    last_used: Instant,
    domain: CacheDomain,
}

#[cfg(not(test))]
#[derive(Clone, Debug, Default)]
struct CacheDomain;

#[cfg(test)]
#[derive(Clone, Debug)]
// New test roots use their creating thread's domain, stored in the root's
// verification state for later publications on workers. Non-test builds share
// one publication-cache domain and enforce MAX_ROOT_CACHES across all roots.
struct CacheDomain(Arc<TestCacheDomain>);

#[cfg(test)]
#[derive(Debug)]
struct TestCacheDomain {
    active: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
struct TestCacheScope {
    domain: CacheDomain,
}

#[cfg(test)]
thread_local! {
    static TEST_CACHE_SCOPE: TestCacheScope = TestCacheScope {
        domain: CacheDomain(Arc::new(TestCacheDomain { active: std::sync::atomic::AtomicBool::new(true) })),
    };
}

#[cfg(test)]
impl Default for CacheDomain {
    fn default() -> Self {
        TEST_CACHE_SCOPE.with(|scope| scope.domain.clone())
    }
}

impl CacheDomain {
    fn same(&self, other: &Self) -> bool {
        #[cfg(test)]
        {
            Arc::ptr_eq(&self.0, &other.0)
        }
        #[cfg(not(test))]
        {
            let _ = other;
            true
        }
    }

    fn active(&self) -> bool {
        #[cfg(test)]
        {
            self.0.active.load(std::sync::atomic::Ordering::Acquire)
        }
        #[cfg(not(test))]
        {
            true
        }
    }
}

#[cfg(test)]
impl Drop for TestCacheScope {
    fn drop(&mut self) {
        self.domain
            .0
            .active
            .store(false, std::sync::atomic::Ordering::Release);
        let retired = {
            let mut caches = caches().lock().unwrap_or_else(|error| error.into_inner());
            let keys = caches
                .iter()
                .filter(|(_, cache)| cache.domain.same(&self.domain))
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| caches.remove(&key))
                .collect::<Vec<_>>()
        };
        drop(retired);
    }
}

fn domain_size(caches: &BTreeMap<PathBuf, RootCache>, domain: &CacheDomain) -> usize {
    #[cfg(test)]
    {
        caches
            .values()
            .filter(|cache| cache.domain.same(domain))
            .count()
    }
    #[cfg(not(test))]
    {
        let _ = domain;
        caches.len()
    }
}

#[cfg(test)]
thread_local! {
    static ADMISSION_OVERRIDES: std::cell::RefCell<BTreeMap<PathBuf, crate::db::file_identity::AdmissionSnapshot>> = const { std::cell::RefCell::new(BTreeMap::new()) };
}

pub(super) fn admission_snapshot(storage: &Path) -> crate::db::file_identity::AdmissionSnapshot {
    #[cfg(test)]
    if let Some(snapshot) =
        ADMISSION_OVERRIDES.with(|overrides| overrides.borrow().get(storage).cloned())
    {
        return snapshot;
    }
    crate::db::file_identity::admission_snapshot(storage)
}

#[cfg(all(test, unix))]
struct AdmissionOverride {
    storage: PathBuf,
}

#[cfg(all(test, unix))]
impl AdmissionOverride {
    fn pin(storage: &Path, snapshot: crate::db::file_identity::AdmissionSnapshot) -> Self {
        ADMISSION_OVERRIDES.with(|overrides| {
            assert!(overrides
                .borrow_mut()
                .insert(storage.to_path_buf(), snapshot)
                .is_none());
        });
        Self {
            storage: storage.to_path_buf(),
        }
    }
}

#[cfg(all(test, unix))]
impl Drop for AdmissionOverride {
    fn drop(&mut self) {
        ADMISSION_OVERRIDES.with(|overrides| {
            overrides.borrow_mut().remove(&self.storage);
        });
    }
}

#[derive(Debug, Default)]
struct VerificationState {
    publications: u64,
    last_full: Option<Instant>,
    mismatch_pending: bool,
    active_preparations: usize,
    owners: usize,
    epoch: u64,
    domain: CacheDomain,
    #[cfg(test)]
    mismatch_count: u64,
}

const MAX_ROOT_CACHES: usize = 16;

fn verification_states() -> &'static Mutex<BTreeMap<PathBuf, Weak<Mutex<VerificationState>>>> {
    static STATES: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<VerificationState>>>>> =
        OnceLock::new();
    STATES.get_or_init(Default::default)
}

fn state_for(view_dir: &Path) -> Arc<Mutex<VerificationState>> {
    let mut states = verification_states()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    states.retain(|_, state| state.strong_count() > 0);
    if let Some(state) = states.get(view_dir).and_then(Weak::upgrade) {
        return state;
    }
    let state = Arc::new(Mutex::new(VerificationState::default()));
    states.insert(view_dir.to_path_buf(), Arc::downgrade(&state));
    state
}

/// The context owns lightweight verification deadlines independently of the
/// bounded heavy cache, so LRU pressure cannot cause endless idle rebuilds.
#[derive(Debug)]
pub(crate) struct CacheLease {
    view_dir: PathBuf,
    state: Arc<Mutex<VerificationState>>,
}

impl CacheLease {
    pub(crate) fn new(view_dir: &Path) -> Self {
        let state = state_for(view_dir);
        state.lock().unwrap_or_else(|e| e.into_inner()).owners += 1;
        Self {
            view_dir: view_dir.to_path_buf(),
            state,
        }
    }
}

pub(super) fn release_cache(view_dir: &Path) {
    let state = state_for(view_dir);
    let retired = {
        let mut caches = caches().lock().unwrap_or_else(|e| e.into_inner());
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        state.epoch = state.epoch.wrapping_add(1);
        caches.remove(view_dir)
    };
    drop(retired);
}

pub(super) fn try_release_cache(view_dir: &Path) -> bool {
    let mut caches = match caches().try_lock() {
        Ok(caches) => caches,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return false,
    };
    let states = match verification_states().try_lock() {
        Ok(states) => states,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return false,
    };
    let state = states.get(view_dir).and_then(Weak::upgrade);
    drop(states);
    if let Some(state) = state {
        let mut state = match state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return false,
        };
        state.epoch = state.epoch.wrapping_add(1);
    }
    let retired = caches.remove(view_dir);
    drop(caches);
    drop(retired);
    true
}

impl Drop for CacheLease {
    fn drop(&mut self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.owners = state.owners.saturating_sub(1);
        }
        let retired = {
            let mut caches = caches().lock().unwrap_or_else(|e| e.into_inner());
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.owners != 0 {
                return;
            }
            state.epoch = state.epoch.wrapping_add(1);
            if caches
                .get(&self.view_dir)
                .is_some_and(|cache| Arc::ptr_eq(&cache.verification, &self.state))
            {
                caches.remove(&self.view_dir)
            } else {
                None
            }
        };
        drop(retired);
        // A lease acquisition may already hold an Arc without having incremented
        // owners. Leave its registry identity intact; state_for prunes dead Weak
        // entries after all leases and preparations release their Arcs.
    }
}

#[derive(Clone, PartialEq, Eq)]
struct IndexStamp {
    path: PathBuf,
    len: u64,
    modified: std::time::SystemTime,
    content_hash: [u8; 32],
}

fn index_stamp(project_root: &Path) -> Option<IndexStamp> {
    let output = std::process::Command::new("git")
        .current_dir(project_root)
        .args(["rev-parse", "--git-path", "index"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let bytes = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    let path = project_root.join(path_from_bytes(bytes));
    let metadata = fs::metadata(&path).ok()?;
    Some(IndexStamp {
        // Hashing index contents also detects replacements that preserve both
        // size and timestamp, including changes to cached Git attributes.
        content_hash: *blake3::hash(&fs::read(&path).ok()?).as_bytes(),
        path,
        len: metadata.len(),
        modified: metadata.modified().ok()?,
    })
}

fn caches() -> &'static Mutex<BTreeMap<PathBuf, RootCache>> {
    static CACHES: OnceLock<Mutex<BTreeMap<PathBuf, RootCache>>> = OnceLock::new();
    CACHES.get_or_init(Default::default)
}

#[cfg(test)]
pub(super) fn cache_contains_for_test(view_dir: &Path) -> bool {
    caches()
        .lock()
        .unwrap()
        .get(view_dir)
        .is_some_and(|cache| cache.previous.is_some())
}

#[cfg(test)]
fn cache_domain_size_for_test() -> usize {
    domain_size(
        &caches().lock().unwrap_or_else(|error| error.into_inner()),
        &CacheDomain::default(),
    )
}

#[cfg(test)]
fn mismatch_count_for_test(view_dir: &Path) -> u64 {
    state_for(view_dir)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .mismatch_count
}

#[cfg(test)]
#[derive(Debug)]
pub(super) struct CacheMemoryEstimate {
    pub payload_bytes: usize,
    pub estimated_bytes: usize,
    pub metadata_size: usize,
    pub head_path_size: usize,
    pub json_bytes: usize,
    pub head_bytes: usize,
    pub path_index_bytes: usize,
}

#[cfg(test)]
pub(super) fn cache_memory_for_test(view_dir: &Path) -> CacheMemoryEstimate {
    use std::mem::size_of;
    let caches = caches().lock().unwrap();
    let cache = &caches[view_dir];
    let previous = cache.previous.as_ref().unwrap();
    let mut manifest_dynamic = 0;
    for (path, entry) in previous.manifest.entries() {
        manifest_dynamic += path.0 .0.capacity();
        manifest_dynamic += match entry {
            ManifestEntry::Regular { planes, .. } => {
                planes.semantic.as_ref().map_or(0, String::capacity)
                    + planes.callgraph.as_ref().map_or(0, String::capacity)
            }
            ManifestEntry::Symlink { target_bytes } => target_bytes.0.capacity(),
            ManifestEntry::Gitlink { oid } => oid.capacity(),
            ManifestEntry::Synthetic { name, planes } => {
                name.capacity() + planes.callgraph.capacity()
            }
        };
    }
    let json_bytes = previous
        .manifest
        .json
        .get()
        .map_or(0, |(_, bytes)| bytes.len());
    let metadata_dynamic = previous.metadata.keys().map(Vec::capacity).sum::<usize>();
    let manifest_inline =
        previous.manifest.entries().count() * size_of::<(RelPath, ManifestEntry)>();
    let metadata_inline = previous.metadata.len() * size_of::<(Vec<u8>, fs::Metadata)>();
    let (head_bytes, path_index_bytes) = cache.head.as_ref().map_or((0, 0), |head| {
        let head_bytes = head.entries.capacity() * size_of::<crate::alias::TrackedPath>()
            + head
                .entries
                .iter()
                .map(|entry| {
                    entry.rel_path.capacity()
                        + match &entry.mode {
                            GitMode::Other(mode) => mode.capacity(),
                            _ => 0,
                        }
                })
                .sum::<usize>();
        let index_bytes = head.paths.get().map_or(0, |paths| {
            [&paths.members, &paths.directories]
                .into_iter()
                .map(|values| {
                    values.capacity() * size_of::<Vec<u8>>()
                        + values.iter().map(Vec::capacity).sum::<usize>()
                })
                .sum()
        });
        let relevant_bytes = head
            .relevant
            .get()
            .and_then(Option::as_ref)
            .map_or(0, |names| {
                names
                    .iter()
                    .map(|name| size_of::<Vec<u8>>() + name.capacity())
                    .sum::<usize>()
            });
        (head_bytes, index_bytes + relevant_bytes)
    });
    let payload_bytes = size_of::<RootCache>()
        + manifest_dynamic
        + manifest_inline
        + metadata_dynamic
        + metadata_inline
        + json_bytes
        + head_bytes
        + path_index_bytes;
    // Approximate B-tree spare slots at eight occupied keys per eleven-slot
    // node, plus parent/child pointers. This is a retained-graph estimate, not RSS.
    let estimated_bytes = payload_bytes
        + (manifest_inline + metadata_inline) * 3 / 8
        + (previous.manifest.entries().count() + previous.metadata.len()).div_ceil(8) * 112;
    CacheMemoryEstimate {
        payload_bytes,
        estimated_bytes,
        metadata_size: size_of::<fs::Metadata>(),
        head_path_size: size_of::<crate::alias::TrackedPath>(),
        json_bytes,
        head_bytes,
        path_index_bytes,
    }
}

pub(super) fn previous(view: &ViewStore, generation: Option<&str>) -> Option<CachedManifest> {
    let mut caches = caches().lock().unwrap_or_else(|e| e.into_inner());
    let cache = caches.get_mut(view.view_dir())?;
    cache.last_used = Instant::now();
    cache
        .previous
        .as_ref()
        .filter(|cached| Some(cached.generation.as_str()) == generation)
        .cloned()
}

pub(super) fn remember(
    view: &ViewStore,
    cached: CachedManifest,
    full_verified: bool,
    preparation: &PreparationVerification,
) {
    let mut caches = caches().lock().unwrap_or_else(|e| e.into_inner());
    let mut state = preparation.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.epoch != preparation.epoch || !state.domain.active() {
        return;
    }
    let domain = state.domain.clone();
    let previous = caches.insert(
        view.view_dir().to_path_buf(),
        RootCache {
            head: cached.head.clone(),
            previous: Some(cached),
            verification: preparation.state.clone(),
            last_used: Instant::now(),
            domain: domain.clone(),
        },
    );
    if full_verified {
        state.publications = 0;
        state.last_full = Some(Instant::now());
        state.mismatch_pending = false;
    }
    drop(state);
    let mut retired = Vec::new();
    while domain_size(&caches, &domain) > MAX_ROOT_CACHES {
        let oldest = caches
            .iter()
            .filter(|(_, cache)| cache.domain.same(&domain))
            .min_by_key(|(_, cache)| cache.last_used)
            .map(|(key, _)| key.clone())
            .unwrap();
        retired.push(caches.remove(&oldest).unwrap());
    }
    drop(caches);
    // Large maps and JSON buffers are destroyed outside the global cache lock.
    drop((previous, retired));
}

/// Rescans also refresh attributes: check-attr reads the index, whose attributes
/// can change without a new HEAD tree. Attribute edits and index replacements
/// must not reuse that cache, even when the watcher reports only a source edit.
pub(super) fn head(request: &AssemblyRequest, view: &ViewStore) -> Result<HeadRead> {
    let output = std::process::Command::new("git")
        .current_dir(&request.project_root)
        .args(["rev-parse", "HEAD^{tree}"])
        .output()?;
    if !output.status.success() {
        return Err(ViewError::InvalidManifest(
            "cannot resolve HEAD tree".into(),
        ));
    }
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let index = index_stamp(&request.project_root);
    let eligible = !request.changed_paths.is_empty()
        && index.is_some()
        && !request.changed_paths.iter().any(|path| {
            path.rsplit(|byte| *byte == b'/').next() == Some(b".gitattributes".as_slice())
        });
    if eligible {
        let snapshot = {
            let mut caches = caches().lock().unwrap_or_else(|e| e.into_inner());
            caches.get_mut(view.view_dir()).and_then(|cache| {
                cache.last_used = Instant::now();
                cache.head.clone()
            })
        };
        if let Some(snapshot) = snapshot {
            if snapshot.tree == tree && Some(&snapshot.index) == index.as_ref() {
                return Ok(HeadRead {
                    tree,
                    entries: snapshot.entries.clone(),
                    paths: snapshot.paths.clone(),
                    cached: true,
                    snapshot: Some(snapshot),
                });
            }
        }
    }
    let entries = Arc::new(
        head_tree_entries(&request.project_root)
            .map_err(|error| ViewError::InvalidManifest(error.to_string()))?,
    );
    let paths = Arc::new(OnceLock::new());
    // This listing is only a candidate. A failed or cancelled preparation must
    // leave the served manifest's index invalidation pending for the retry.
    let snapshot = index.map(|index| HeadSnapshot {
        tree: tree.clone(),
        index,
        entries: entries.clone(),
        paths: paths.clone(),
        relevant: Arc::new(OnceLock::new()),
    });
    Ok(HeadRead {
        tree,
        entries,
        cached: false,
        snapshot,
        paths,
    })
}

fn named_paths_have_single_link(request: &AssemblyRequest) -> bool {
    request.changed_paths.iter().all(|path| {
        // The metadata helper falls back to an unknown identity and a best-effort
        // count on Windows errors. Only an observed count can prove one link.
        matches!(
            crate::file_identity::metadata_with_link_count(
                &request.project_root.join(path_from_bytes(path))
            ),
            Ok((_, crate::file_identity::FileIdentity::Known { .. }, 1))
        )
    })
}

/// Only a successful publication can supply the HEAD and artifact evidence for
/// discarding an irrelevant notification. Unknown state keeps the normal scan.
pub(super) fn irrelevant_delta(
    request: &AssemblyRequest,
    view: &ViewStore,
    complete: impl FnOnce() -> Result<bool>,
) -> Result<bool> {
    if request.changed_paths.is_empty() || request.require_semantic {
        return Ok(false);
    }
    let proof = {
        let mut caches = caches().lock().unwrap_or_else(|e| e.into_inner());
        caches.get_mut(view.view_dir()).and_then(|cache| {
            cache.last_used = Instant::now();
            let previous = cache.previous.as_ref()?;
            let state = cache.verification.lock().unwrap_or_else(|e| e.into_inner());
            if !cache.domain.active()
                || state.active_preparations != 0
                || full_verification_required(&state, Instant::now())
                || (request.callgraph && manifest_lacks_callgraph(&previous.manifest))
            {
                return None;
            }
            Some((
                previous.generation.clone(),
                previous.head.clone()?,
                previous.stamps.clone(),
                previous.admission.clone(),
                previous.metadata.clone(),
                cache.verification.clone(),
                (state.epoch, state.publications, state.last_full),
            ))
        })
    };
    let Some((generation, head, stamps, admission, metadata, state, revision)) = proof else {
        return Ok(false);
    };
    if !super::super::generation_matches_head(&generation, &request.desired_head) {
        return Ok(false);
    }
    // Case-fold ASCII names even on case-sensitive hosts: the same watcher
    // batches can come from case-insensitive checkouts. Non-ASCII spellings,
    // host path aliases and control files are uncertain, never evidence for a skip.
    let Some(relevant) = relevant_names(&head) else {
        return Ok(false);
    };
    if request.changed_paths.iter().any(|path| {
        let lower = path.to_ascii_lowercase();
        uncertain_spelling(path)
            || is_resolution_input(path)
            || is_resolution_input(&lower)
            || matches!(
                lower.rsplit(|byte| *byte == b'/').next(),
                Some(b".gitattributes" | b".aftignore" | b".ignore")
            )
            || relevant.contains(&path.to_ascii_lowercase())
    }) {
        return Ok(false);
    }
    // A nonmember name can be a new hard link to a tracked member. Its own
    // current link count must be checked even when cached member stats are valid.
    if !named_paths_have_single_link(request) {
        return Ok(false);
    }
    let tree = std::process::Command::new("git")
        .current_dir(&request.project_root)
        .args(["rev-parse", "HEAD^{tree}"])
        .output()?;
    if !tree.status.success()
        || String::from_utf8_lossy(&tree.stdout).trim() != head.tree
        || index_stamp(&request.project_root).as_ref() != Some(&head.index)
        || view.current_generation_read_only()?.as_deref() != Some(&generation)
    {
        return Ok(false);
    }
    let callgraph = BlobStore::open(
        &request.storage,
        request.family.clone(),
        BlobPlane::Callgraph,
    )
    .map_err(store_error)?;
    let semantic = BlobStore::open(
        &request.storage,
        request.family.clone(),
        BlobPlane::Semantic,
    )
    .map_err(store_error)?;
    if StoreStamps::read(&callgraph, &semantic)? != stamps {
        return Ok(false);
    }
    // Other roots open and close databases too. A global admission change
    // requires fresh identity checks for this HEAD, not a manifest rebuild for
    // every root. Recheck live registrations even when cached identities suffice.
    let admission_changed = admission_snapshot(&request.storage) != admission;
    for entry in head.entries.iter() {
        let absolute = request.project_root.join(path_from_bytes(&entry.rel_path));
        let Some(cached) = metadata
            .get(&entry.rel_path)
            .filter(|metadata| metadata.is_file())
        else {
            return Ok(false);
        };
        let fresh;
        let current = if admission_changed {
            let Ok(value) = crate::db::file_identity::candidate_metadata(&absolute) else {
                return Ok(false);
            };
            fresh = value;
            if !fresh.is_file()
                || fresh.len() != cached.len()
                || fresh.modified().ok() != cached.modified().ok()
            {
                return Ok(false);
            }
            &fresh
        } else {
            cached
        };
        if crate::project_content::is_excluded_event(&absolute)
            || crate::db::file_identity::is_live_sqlite_file_with_metadata(&absolute, Some(current))
        {
            return Ok(false);
        }
    }
    if !complete()? || view.current_generation_read_only()?.as_deref() != Some(&generation) {
        return Ok(false);
    }
    // A preparation overlapping this probe revokes its conclusion. Only hold
    // the per-view lock for the final decision, never across filesystem I/O.
    let state = state.lock().unwrap_or_else(|e| e.into_inner());
    Ok(state.active_preparations == 0
        && state.domain.active()
        && (state.epoch, state.publications, state.last_full) == revision
        && !full_verification_required(&state, Instant::now()))
}

pub(super) struct PreparationVerification {
    state: Arc<Mutex<VerificationState>>,
    epoch: u64,
    pub due: bool,
}

impl Drop for PreparationVerification {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active_preparations = state.active_preparations.saturating_sub(1);
    }
}

pub(super) fn begin_preparation(view_dir: &Path) -> PreparationVerification {
    let state = state_for(view_dir);
    let mut verification = state.lock().unwrap_or_else(|e| e.into_inner());
    verification.publications = verification.publications.saturating_add(1);
    verification.active_preparations += 1;
    PreparationVerification {
        state: state.clone(),
        epoch: verification.epoch,
        due: full_verification_required(&verification, Instant::now()),
    }
}

const FULL_VERIFICATION_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

// Failed preparations leave both the count and deadline due. Only remembering
// a successfully served full result (including an identical no-op) renews them.
fn full_verification_required(cache: &VerificationState, now: Instant) -> bool {
    cache.publications >= 64
        || cache
            .last_full
            .is_none_or(|last| now.saturating_duration_since(last) >= FULL_VERIFICATION_INTERVAL)
}

pub(super) fn full_verification_due(view_dir: &Path) -> bool {
    // The registry protects only map metadata, never per-view locks or I/O.
    // Wait for that short lookup: another view's registry activity is not
    // evidence that this view needs a full verification.
    let states = verification_states()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(state) = states.get(view_dir).and_then(Weak::upgrade) else {
        return true;
    };
    drop(states);
    let state = match state.try_lock() {
        Ok(state) => state,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return true,
    };
    state.domain.active()
        && state.active_preparations == 0
        && full_verification_required(&state, Instant::now())
}

pub(super) fn note_mismatch(view: &ViewStore) -> Option<u64> {
    let state = state_for(view.view_dir());
    let mut cache = state.lock().unwrap_or_else(|e| e.into_inner());
    if cache.mismatch_pending {
        return None;
    }
    cache.mismatch_pending = true;
    #[cfg(test)]
    {
        cache.mismatch_count += 1;
    }
    Some(VERIFICATION_MISMATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1)
}

#[cfg(test)]
pub(super) fn expire_verification_for_test(view_dir: &Path) {
    let state = state_for(view_dir);
    let mut cache = state.lock().unwrap();
    cache.last_full = Some(Instant::now() - FULL_VERIFICATION_INTERVAL);
}

pub(super) static VERIFICATION_MISMATCHES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(super) fn full_candidates(
    reader: &CandidateReader<'_>,
    head: &[crate::alias::TrackedPath],
    skips: &mut Vec<(Vec<u8>, Option<SkipReason>)>,
    reads: &mut std::collections::HashSet<Vec<u8>>,
    phase: &mut impl FnMut(&str) -> Result<()>,
) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for tracked in head.iter().cloned() {
        let path = RelPath::new(tracked.rel_path.clone())?;
        let absolute = reader
            .request
            .project_root
            .join(path_from_bytes(&tracked.rel_path));
        if crate::project_content::is_excluded(&absolute) {
            continue;
        }
        if matches!(tracked.mode, GitMode::Regular { .. } | GitMode::Symlink)
            && working_tree_io(
                fs::symlink_metadata(&absolute),
                "reading metadata for",
                &absolute,
            )?
            .is_none()
        {
            continue;
        }
        let entry = match tracked.mode {
            GitMode::Regular { executable } => {
                phase("working_tree_read")?;
                let Some(source) = read_working_tree_file(&absolute)? else {
                    continue;
                };
                reads.insert(tracked.rel_path.clone());
                let (candidate, skip) =
                    reader.regular(path, &absolute, executable, source, Some(tracked), true)?;
                skips.extend(skip);
                candidates.push(candidate);
                continue;
            }
            GitMode::Symlink => {
                phase("working_tree_read")?;
                let Some(target) = read_symlink_bytes(&absolute)? else {
                    continue;
                };
                ManifestEntry::Symlink {
                    target_bytes: ByteString::new(target),
                }
            }
            GitMode::Gitlink => ManifestEntry::Gitlink {
                oid: tracked.git_oid.to_hex(),
            },
            GitMode::Other(_) => {
                skips.push((tracked.rel_path, Some(SkipReason::UnsupportedGitMode)));
                continue;
            }
        };
        candidates.push(Candidate {
            path,
            entry,
            key: None,
            payload: None,
            tracked: None,
            source: None,
        });
    }
    Ok(candidates)
}

/// The closure borrows the stores already opened by assembly. In particular,
/// reading removal counters never opens another descriptor on a live database.
pub(super) struct StoreClosure<'a> {
    pub callgraph: &'a BlobStore,
    pub semantic: &'a BlobStore,
    pub trigram: PathBuf,
    pub trusted: Option<(&'a Manifest, StoreStamps)>,
}

impl PublicationClosure for StoreClosure<'_> {
    fn contains_blob(&self, plane: ArtifactPlane, key: &str) -> Result<bool> {
        let store = if plane == ArtifactPlane::Semantic {
            self.semantic
        } else {
            self.callgraph
        };
        Ok(store
            .missing_hex_keys([key], None)
            .map_err(store_error)?
            .missing
            .is_empty())
    }

    fn probe_blobs(&self, keys: &[(ArtifactPlane, &str)]) -> Result<()> {
        #[cfg(test)]
        let started = Instant::now();
        #[cfg(test)]
        let before = crate::blob_store::MEMBERSHIP_PROBE_STATS.with(|stats| stats.get().queries);
        // This runs under the publication durability barrier, after assembly's
        // puts. A removal in the meantime revokes the entire base's proof.
        let stamps = StoreStamps::read(self.callgraph, self.semantic)?;
        let old = self
            .trusted
            .as_ref()
            .filter(|(_, before)| *before == stamps)
            .map(|(manifest, _)| {
                manifest
                    .plane_keys()
                    .map(|(plane, key)| (plane == ArtifactPlane::Semantic, key))
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        for plane in [ArtifactPlane::Semantic, ArtifactPlane::Callgraph] {
            let store = if plane == ArtifactPlane::Semantic {
                self.semantic
            } else {
                self.callgraph
            };
            let missing = store
                .missing_hex_keys(
                    keys.iter()
                        .filter(|(p, key)| {
                            *p == plane && !old.contains(&(*p == ArtifactPlane::Semantic, *key))
                        })
                        .map(|(_, key)| *key),
                    None,
                )
                .map_err(store_error)?
                .missing;
            if let Some((_, key)) = keys
                .iter()
                .find(|(p, key)| *p == plane && missing.contains(*key))
            {
                return Err(ViewError::MissingBlob {
                    plane,
                    key: (*key).to_owned(),
                });
            }
        }
        #[cfg(test)]
        CLOSURE_PROBE_STATS.with(|stats| {
            let after = crate::blob_store::MEMBERSHIP_PROBE_STATS.with(|stats| stats.get().queries);
            crate::blob_store::ProbeStats::add(stats, after - before, started);
        });
        Ok(())
    }

    fn trigram_is_present(&self) -> Result<bool> {
        Ok(self.trigram.is_file())
    }
    fn contains_alias(&self, _git_oid: &str) -> Result<bool> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_cache_domains_isolate_parallel_root_pressure() {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let other = std::thread::spawn(move || {
            let (_project, storage, request) = fixture();
            let view = ViewStore::open(storage.path(), &request.scope).unwrap();
            ready_tx.send(()).unwrap();
            done_rx.recv().unwrap();
            assert!(
                cache_contains_for_test(view.view_dir()),
                "a sibling test's roots evicted this test's warm entry"
            );
        });
        ready_rx.recv().unwrap();
        let mut roots = Vec::new();
        for _ in 0..32 {
            roots.push(fixture());
        }
        assert!(cache_domain_size_for_test() <= 16);
        done_tx.send(()).unwrap();
        other.join().unwrap();
    }

    #[test]
    fn views_large_watcher_delta_has_bounded_path_comparisons() {
        let oid = crate::alias::GitOid::from_bytes(&[0; 20]).unwrap();
        let head = (0..60_000)
            .map(|index| crate::alias::TrackedPath {
                rel_path: format!("src/p{:04}/f{index:05}.rs", index / 100).into_bytes(),
                mode: GitMode::Regular { executable: false },
                git_oid: oid,
                filtered: false,
                present_in_previous_generation: true,
                tracked: true,
            })
            .collect::<Vec<_>>();
        let changed = head
            .iter()
            .skip(head.len() - 4096)
            .map(|entry| entry.rel_path.clone())
            .collect::<BTreeSet<_>>();
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                PATH_COMPARISONS.with(|stats| stats.set((0, None)));
            }
        }
        let _reset = Reset;
        PATH_COMPARISONS.with(|stats| stats.set((0, Some(4_000_000))));
        let paths = OnceLock::new();
        assert_eq!(
            delta_read_scope(&head, &paths, &changed, |_| false),
            Some(BTreeSet::new())
        );
        let comparisons = PATH_COMPARISONS.with(|stats| stats.get().0);
        assert!(
            comparisons > changed.len(),
            "the counter must observe actual path comparisons"
        );
        println!(
            "watcher_delta head={} changed={} comparisons={comparisons}",
            head.len(),
            changed.len()
        );
        PATH_COMPARISONS.with(|stats| stats.set((0, Some(200_000))));
        assert_eq!(
            delta_read_scope(&head, &paths, &changed, |_| false),
            Some(BTreeSet::new())
        );
        println!(
            "watcher_delta warm_comparisons={}",
            PATH_COMPARISONS.with(|stats| stats.get().0)
        );
    }

    #[test]
    fn views_full_rebuild_skips_delta_comparison_index() {
        let (_project, storage, request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        assert!(caches()
            .lock()
            .unwrap()
            .get(view.view_dir())
            .unwrap()
            .head
            .as_ref()
            .unwrap()
            .paths
            .get()
            .is_none());
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                PATH_COMPARISONS.with(|stats| stats.set((0, None)));
            }
        }
        let _reset = Reset;
        PATH_COMPARISONS.with(|stats| stats.set((0, Some(0))));
        publish(&request);
        assert_eq!(PATH_COMPARISONS.with(|stats| stats.get().0), 0);
    }

    #[test]
    fn views_root_cache_cap_releases_lru_payloads() {
        let mut roots = Vec::new();
        for _ in 0..16 {
            roots.push(fixture());
        }
        let view =
            |index: usize| ViewStore::open(roots[index].1.path(), &roots[index].2.scope).unwrap();
        let first = view(0);
        let second = view(1);
        let weak = {
            let caches = caches().lock().unwrap();
            caches
                .get(second.view_dir())
                .and_then(|cache| cache.previous.as_ref())
                .map(|previous| Arc::downgrade(&previous.metadata))
        }
        .expect("the second root must remain in this test's cache domain");
        let generation = first.current_generation().unwrap().unwrap();
        drop(previous(&first, Some(&generation)).unwrap());
        roots.push(fixture());
        assert!(
            cache_domain_size_for_test() <= 16,
            "heavy root cache must remain bounded"
        );
        assert!(
            cache_contains_for_test(first.view_dir()),
            "recently used entry must survive LRU pressure"
        );
        assert!(!cache_contains_for_test(second.view_dir()));
        assert!(
            weak.upgrade().is_none(),
            "eviction must release metadata, not just hide its key"
        );
        for _ in 0..48 {
            roots.push(fixture());
        }
        assert!(cache_domain_size_for_test() <= 16);
    }

    #[test]
    fn views_lru_keeps_live_context_deadlines_and_release_rejects_old_preparations() {
        let (project, storage, mut request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        let lease = CacheLease::new(view.view_dir());
        let mut roots = Vec::new();
        for _ in 0..20 {
            roots.push(fixture());
        }
        assert!(!cache_contains_for_test(view.view_dir()));
        assert!(
            !full_verification_due(view.view_dir()),
            "LRU pressure must not reset a live context's idle deadline"
        );
        fs::write(project.path().join("f0.rs"), "pub fn changed() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        let mut prepared = prepare_checkout_deferring(&request, true, &mut |_| Ok(())).unwrap();
        release_cache(view.view_dir());
        assert!(prepared.commit().unwrap().published);
        assert!(
            !cache_contains_for_test(view.view_dir()),
            "a pre-release preparation must not resurrect the cache"
        );
        drop(prepared);
        drop(lease);
        assert!(full_verification_due(view.view_dir()));
    }

    #[test]
    fn views_last_runtime_lease_releases_its_cache() {
        let (_project, storage, request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        let first = CacheLease::new(view.view_dir());
        let second = CacheLease::new(view.view_dir());
        let weak = {
            let caches = caches().lock().unwrap();
            Arc::downgrade(
                &caches
                    .get(view.view_dir())
                    .unwrap()
                    .previous
                    .as_ref()
                    .unwrap()
                    .metadata,
            )
        };
        drop(first);
        assert!(
            cache_contains_for_test(view.view_dir()),
            "another installed runtime still owns the root"
        );
        drop(second);
        assert!(!cache_contains_for_test(view.view_dir()));
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn views_in_flight_state_lookup_keeps_lease_identity() {
        let directory = tempfile::tempdir().unwrap();
        let first = CacheLease::new(directory.path());
        // A new lease or preparation can have upgraded the registry's Weak
        // entry without acquiring the per-view state lock yet.
        let in_flight = state_for(directory.path());
        drop(first);
        let next = CacheLease::new(directory.path());
        assert!(
            Arc::ptr_eq(&in_flight, &next.state),
            "a live state lookup must not split one view's verification identity"
        );
    }

    #[test]
    fn views_release_before_view_open_rejects_the_preparation() {
        let (project, storage, mut request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        let _lease = CacheLease::new(view.view_dir());
        fs::write(project.path().join("f0.rs"), "pub fn changed() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        let mut released = false;
        let mut prepared = prepare_checkout_deferring(&request, true, &mut |phase| {
            if phase == "manifest" && !released {
                released = true;
                release_cache(view.view_dir());
            }
            Ok(())
        })
        .unwrap();
        assert!(released);
        assert!(prepared.commit().unwrap().published);
        assert!(!cache_contains_for_test(view.view_dir()), "a release during preparation admission must not be consumed before its token is captured");
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, AssemblyRequest) {
        // Case-only renames exercise ordinary checkout reads, not the stricter
        // task_temp_reads admission rules for operating-system scratch files.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".cortexkit/scratch/view-publication-fixtures");
        fs::create_dir_all(&root).unwrap();
        fixture_at(Some(&root.canonicalize().unwrap()))
    }

    fn fixture_at(
        parent: Option<&Path>,
    ) -> (tempfile::TempDir, tempfile::TempDir, AssemblyRequest) {
        let tempdir = || {
            parent
                .map_or_else(tempfile::tempdir, tempfile::tempdir_in)
                .unwrap()
        };
        let project = tempdir();
        let storage = tempdir();
        git(project.path(), &["init", "--quiet"]);
        assert!(project.path().join(".git").is_dir());
        let output = std::process::Command::new("git")
            .current_dir(project.path())
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let bytes = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        assert_eq!(
            path_from_bytes(bytes).canonicalize().unwrap(),
            project.path().canonicalize().unwrap(),
            "fixture must not discover its enclosing checkout"
        );
        for i in 0..8 {
            fs::write(
                project.path().join(format!("f{i}.rs")),
                format!("pub fn f{i}() {{}}\n"),
            )
            .unwrap();
        }
        git(project.path(), &["add", "."]);
        git(project.path(), &["commit", "--quiet", "-m", "base"]);
        let request = AssemblyRequest {
            project_root: project.path().to_path_buf(),
            storage: storage.path().to_path_buf(),
            family: "incremental".into(),
            scope: "incremental".into(),
            desired_head: "head".into(),
            changed_paths: BTreeSet::new(),
            semantic_keys: BTreeMap::new(),
            require_semantic: false,
            allow_blob_put: true,
            callgraph: true,
        };
        publish(&request);
        (project, storage, request)
    }

    fn publish(request: &AssemblyRequest) -> Manifest {
        let mut prepared = prepare_checkout_deferring(request, true, &mut |_| Ok(())).unwrap();
        let manifest = prepared.report().manifest.clone().unwrap_or_else(|| {
            ViewStore::open(&request.storage, &request.scope)
                .unwrap()
                .load_manifest(prepared.report().generation.as_deref().unwrap())
                .unwrap()
        });
        prepared.commit().unwrap();
        prepared.settle_status();
        manifest
    }

    fn rebuild_manifest(request: &AssemblyRequest) -> Manifest {
        let callgraph =
            BlobStore::open(&request.storage, &request.family, BlobPlane::Callgraph).unwrap();
        let semantic =
            BlobStore::open(&request.storage, &request.family, BlobPlane::Semantic).unwrap();
        let reader = CandidateReader {
            request,
            previous: None,
            callgraph: &callgraph,
        };
        let mut skips = Vec::new();
        let mut reads = std::collections::HashSet::new();
        let head = head_tree_entries(&request.project_root).unwrap();
        let mut candidates =
            full_candidates(&reader, &head, &mut skips, &mut reads, &mut |_| Ok(())).unwrap();
        heal_missing_payloads(
            &reader,
            &semantic,
            &mut candidates,
            &mut skips,
            &mut reads,
            None,
            &mut |_| Ok(()),
        )
        .unwrap();
        Manifest::new(
            candidates
                .into_iter()
                .map(|candidate| (candidate.path, candidate.entry)),
        )
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn views_incremental_reuses_manifest_and_head_without_decoding() {
        assert_warm_reuse(fixture());
    }

    #[cfg(unix)]
    #[test]
    fn views_fixture_owns_git_state_inside_another_checkout() {
        let parent = tempfile::tempdir().unwrap();
        git(parent.path(), &["init", "--quiet"]);
        fs::write(parent.path().join("parent.rs"), "pub fn parent() {}\n").unwrap();
        git(parent.path(), &["add", "."]);
        git(parent.path(), &["commit", "--quiet", "-m", "parent"]);
        assert_warm_reuse(fixture_at(Some(parent.path())));
    }

    #[cfg(unix)]
    fn assert_warm_reuse(
        (project, _storage, mut request): (tempfile::TempDir, tempfile::TempDir, AssemblyRequest),
    ) {
        let view = ViewStore::open(&request.storage, &request.scope).unwrap();
        let generation = view.current_generation().unwrap().unwrap();
        let cached = previous(&view, Some(&generation)).unwrap();
        // Pin the cached AdmissionSnapshot so unrelated SQLite opens/closes in
        // parallel tests cannot legitimately force fresh metadata reads here.
        // Alias-invalidation tests exercise the real registry without an override.
        let _admission = AdmissionOverride::pin(&request.storage, cached.admission.clone());
        drop(cached);
        // Force ambient registry churn so accidentally using the process-wide
        // snapshot cannot silently make this isolation check pass.
        let external = tempfile::tempdir().unwrap();
        let _external_db = crate::db::file_identity::IdentityConnection::open(
            external.path().join("external.sqlite"),
            "publication reuse fixture",
        )
        .unwrap();
        crate::views::take_manifest_decodes();
        CLOSURE_PROBE_STATS.with(|stats| stats.set(Default::default()));
        let heads = crate::alias::head_tree_entry_calls_for_test();
        fs::write(project.path().join("f0.rs"), "pub fn edited() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        let mut prepared = None;
        let stats = crate::db::file_identity::stat_probe::count_candidates(|| {
            prepared = Some(prepare_checkout_deferring(&request, true, &mut |_| Ok(())).unwrap());
        });
        let mut prepared = prepared.unwrap();
        assert_eq!(crate::views::take_manifest_decodes(), 0);
        assert_eq!(crate::alias::head_tree_entry_calls_for_test(), heads);
        assert_eq!(CLOSURE_PROBE_STATS.with(|stats| stats.get().queries), 1);
        assert!(
            !stats.contains_key(&project.path().join("f1.rs")),
            "unchanged regular file was statted: {stats:?}"
        );
        assert!(prepared.commit().unwrap().published);
    }

    #[test]
    fn views_incremental_index_attribute_change_refreshes_head_cache() {
        let (project, storage, mut request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        assert!(head(&request, &view).unwrap().cached);
        fs::write(project.path().join(".gitattributes"), "*.rs filter=lfs\n").unwrap();
        git(project.path(), &["add", ".gitattributes"]);
        let head = head(&request, &view).unwrap();
        assert!(
            !head.cached,
            "staged attributes changed without a HEAD move"
        );
        assert!(head.entries.iter().all(|entry| entry.filtered));
    }

    #[test]
    fn views_failed_preparation_does_not_consume_index_invalidation() {
        let (project, _storage, mut request) = fixture();
        fs::write(project.path().join("f0.rs"), "pub fn dirty() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        publish(&request);
        git(project.path(), &["reset", "--quiet", "--hard", "HEAD"]);
        // A same-tree index rewrite must remain unvalidated after failure.
        fs::write(project.path().join(".gitattributes"), "*.rs text\n").unwrap();
        git(project.path(), &["add", ".gitattributes"]);
        fs::write(project.path().join("f1.rs"), "pub fn reported() {}\n").unwrap();
        request.changed_paths = BTreeSet::from([b"f1.rs".to_vec()]);
        let failure = prepare_checkout_deferring(&request, true, &mut |phase| {
            if phase == "working_tree_read" {
                Err(ViewError::InvalidManifest(
                    "injected post-HEAD failure".into(),
                ))
            } else {
                Ok(())
            }
        });
        assert!(failure.is_err());
        assert_eq!(
            publish(&request).to_json_bytes().unwrap(),
            rebuild_manifest(&request).to_json_bytes().unwrap(),
            "the retry must read the reset file, not reuse its pre-reset dirty key"
        );
    }

    #[test]
    fn views_head_snapshot_waits_for_commit_even_for_a_no_op() {
        let (project, storage, mut request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        let before = caches()
            .lock()
            .unwrap()
            .get(view.view_dir())
            .unwrap()
            .head
            .as_ref()
            .unwrap()
            .index
            .clone();
        fs::write(project.path().join(".gitattributes"), "*.rs text\n").unwrap();
        git(project.path(), &["add", ".gitattributes"]);
        request.changed_paths.insert(b"f0.rs".to_vec());
        let prepared = prepare_checkout_deferring(&request, true, &mut |_| Ok(())).unwrap();
        assert!(
            caches()
                .lock()
                .unwrap()
                .get(view.view_dir())
                .unwrap()
                .head
                .as_ref()
                .unwrap()
                .index
                == before
        );
        drop(prepared);
        assert!(!head(&request, &view).unwrap().cached);
        let mut prepared = prepare_checkout_deferring(&request, true, &mut |_| Ok(())).unwrap();
        assert!(!prepared.commit().unwrap().published);
        assert!(head(&request, &view).unwrap().cached);
    }

    #[cfg(unix)]
    #[test]
    fn views_incremental_cached_identity_rejects_new_live_database_alias() {
        let (project, storage, mut request) = fixture();
        let database = storage.path().join("inactive.sqlite");
        Connection::open(&database)
            .unwrap()
            .execute_batch("CREATE TABLE fixture(value)")
            .unwrap();
        fs::copy(&database, project.path().join("f7.rs")).unwrap();
        publish(&request);
        let alias = storage.path().join("live.sqlite");
        fs::hard_link(project.path().join("f7.rs"), &alias).unwrap();
        let _live =
            crate::db::file_identity::IdentityConnection::open(&alias, "manifest identity test")
                .unwrap();
        fs::write(project.path().join("f0.rs"), "pub fn edited() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        let manifest = publish(&request);
        assert!(manifest
            .get(&RelPath::new(b"f7.rs".to_vec()).unwrap())
            .is_none());
    }

    #[test]
    fn views_incremental_periodic_verification_repairs_missed_change() {
        let (project, storage, mut request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        state_for(view.view_dir()).lock().unwrap().publications = 63;
        let before = mismatch_count_for_test(view.view_dir());
        fs::write(project.path().join("f1.rs"), "pub fn missed() {}\n").unwrap();
        fs::write(project.path().join("f0.rs"), "pub fn notified() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        let incremental = publish(&request).to_json_bytes().unwrap();
        assert!(mismatch_count_for_test(view.view_dir()) > before);
        request.changed_paths.clear();
        assert_eq!(incremental, publish(&request).to_json_bytes().unwrap());
    }

    #[test]
    fn views_periodic_verification_survives_nonincremental_and_failed_preparations() {
        for fail_after_compare in [false, true] {
            let (project, storage, mut request) = fixture();
            let view = ViewStore::open(storage.path(), &request.scope).unwrap();
            state_for(view.view_dir()).lock().unwrap().publications = 63;
            let before = mismatch_count_for_test(view.view_dir());
            fs::write(project.path().join("f1.rs"), "pub fn missed() {}\n").unwrap();
            fs::write(project.path().join("f0.rs"), "pub fn notified() {}\n").unwrap();
            request.changed_paths.insert(b"f0.rs".to_vec());
            if fail_after_compare {
                let failure = prepare_checkout_deferring(&request, true, &mut |phase| {
                    if phase == "cas" {
                        Err(ViewError::InvalidManifest(
                            "injected pre-commit failure".into(),
                        ))
                    } else {
                        Ok(())
                    }
                });
                assert!(failure.is_err());
            } else {
                let generation = view.current_generation().unwrap().unwrap();
                let manifest = view.load_manifest(&generation).unwrap();
                let key = manifest_entry_callgraph_key(
                    manifest
                        .get(&RelPath::new(b"f7.rs".to_vec()).unwrap())
                        .unwrap(),
                )
                .unwrap();
                let store =
                    BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
                let path = store.path().to_path_buf();
                drop(store);
                Connection::open(path)
                    .unwrap()
                    .execute(
                        "DELETE FROM blob_payloads WHERE full_key = ?1",
                        [decode_hex(key).unwrap()],
                    )
                    .unwrap();
            }
            let actual = publish(&request).to_json_bytes().unwrap();
            assert_eq!(
                actual,
                rebuild_manifest(&request).to_json_bytes().unwrap(),
                "fail_after_compare={fail_after_compare}"
            );
            assert_eq!(
                mismatch_count_for_test(view.view_dir()) - before,
                1,
                "one mismatch incident must log once, including across a failed preparation"
            );
        }
    }

    #[test]
    fn views_full_verification_deadline_and_restart_are_bounded() {
        let (project, storage, mut request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        {
            let state = state_for(view.view_dir());
            let cache = state.lock().unwrap();
            let verified = cache.last_full.unwrap();
            assert!(!full_verification_required(
                &cache,
                verified + std::time::Duration::from_secs(599)
            ));
            assert!(full_verification_required(
                &cache,
                verified + std::time::Duration::from_secs(600)
            ));
        }
        expire_verification_for_test(view.view_dir());
        assert!(full_verification_due(view.view_dir()));
        fs::write(
            project.path().join("f1.rs"),
            "pub fn missed_at_deadline() {}\n",
        )
        .unwrap();
        fs::write(project.path().join("f0.rs"), "pub fn notified() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        assert_eq!(
            publish(&request).to_json_bytes().unwrap(),
            rebuild_manifest(&request).to_json_bytes().unwrap()
        );
        assert!(!full_verification_due(view.view_dir()));
        // A new process has no remembered deadline or manifest proof.
        caches().lock().unwrap().remove(view.view_dir());
        assert!(full_verification_due(view.view_dir()));
        fs::write(
            project.path().join("f1.rs"),
            "pub fn missed_while_stopped() {}\n",
        )
        .unwrap();
        assert_eq!(
            publish(&request).to_json_bytes().unwrap(),
            rebuild_manifest(&request).to_json_bytes().unwrap()
        );
        assert!(!full_verification_due(view.view_dir()));
    }

    #[test]
    fn views_other_view_verification_does_not_wake_an_idle_root() {
        use crate::context::{AppContext, ViewRuntimeSnapshot};
        use std::sync::mpsc;
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let view_dir = directory.path().join("view");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&view_dir).unwrap();
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.clone());
        config.views.enabled = true;
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.set_canonical_cache_root(root.clone());
        ctx.install_view_runtime(
            ViewRuntimeSnapshot {
                query_pin: None,
                storage: directory.path().to_owned(),
                family: "verification-probe".into(),
                scope: "verification-probe".into(),
                view_dir: view_dir.clone(),
                generation: Some("verified".into()),
                manifest: None,
                head_fingerprint: String::new(),
                head_metadata: crate::alias::GitHeadMetadata {
                    head_path: root.join(".git/HEAD"),
                    head_mtime: None,
                    resolved_ref_path: None,
                    resolved_ref_mtime: None,
                },
                pending_paths: BTreeSet::new(),
                pending_inputs: BTreeMap::new(),
            },
            None,
        );
        let own_state = state_for(&view_dir);
        own_state.lock().unwrap().last_full = Some(Instant::now());
        crate::runtime_drain::drain_watcher_events(&ctx);
        assert!(!ctx.watcher_drain_has_work());

        let _other_preparation = begin_preparation(&directory.path().join("other-view"));
        std::thread::scope(|scope| {
            let states = verification_states().lock().unwrap();
            let (started_tx, started_rx) = mpsc::channel();
            let (result_tx, result_rx) = mpsc::channel();
            let probe_ctx = &ctx;
            let probe = scope.spawn(move || {
                started_tx.send(()).unwrap();
                result_tx.send(probe_ctx.watcher_drain_has_work()).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            // Keep another view's registry operation in flight until the probe
            // either answers or waits for the registry. Release it before
            // asserting so a failed assertion cannot poison the shared map.
            let early = result_rx.recv_timeout(Duration::from_millis(100));
            drop(states);
            let due =
                early.unwrap_or_else(|_| result_rx.recv_timeout(Duration::from_secs(5)).unwrap());
            probe.join().unwrap();
            assert!(
                !due,
                "another view's registry operation must not enqueue a watcher drain for an idle root"
            );
        });

        let _held_own_state = own_state.lock().unwrap();
        assert!(
            ctx.watcher_drain_has_work(),
            "contention on this view's verification state must still count as pending work"
        );
    }

    #[test]
    fn views_incremental_counter_forces_heal_after_external_delete() {
        let (project, storage, mut request) = fixture();
        let store = BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        let before = store.removal_stamp().unwrap();
        let path = store.path().to_path_buf();
        drop(store);
        // Simulate a maintenance writer bypassing BlobStore, with no second
        // descriptor kept open while the publisher owns the database.
        Connection::open(&path)
            .unwrap()
            .execute_batch("DELETE FROM blob_payloads")
            .unwrap();
        let store = BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        assert_ne!(before, store.removal_stamp().unwrap());
        drop(store);
        fs::write(project.path().join("f0.rs"), "pub fn edited() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        let manifest = publish(&request);
        let store = BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        assert!(store
            .missing_hex_keys(manifest.plane_keys().map(|(_, key)| key), None)
            .unwrap()
            .missing
            .is_empty());
        assert_eq!(manifest.entries().count(), 8);
    }

    #[test]
    fn views_incremental_store_recreation_revokes_stamp() {
        let (project, storage, mut request) = fixture();
        let store = BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        let before = store.removal_stamp().unwrap();
        let path = store.path().to_path_buf();
        drop(store);
        fs::write(&path, b"not a SQLite database").unwrap();
        let store = BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        assert_ne!(before, store.removal_stamp().unwrap());
        drop(store);
        fs::write(project.path().join("f0.rs"), "pub fn edited() {}\n").unwrap();
        request.changed_paths.insert(b"f0.rs".to_vec());
        assert_eq!(publish(&request).entries().count(), 8);
    }

    #[test]
    fn views_directory_events_remove_deleted_and_renamed_subtrees() {
        for rename in [false, true] {
            let (project, _storage, mut request) = fixture();
            fs::create_dir_all(project.path().join("group/nested")).unwrap();
            fs::write(project.path().join("group/a.rs"), "pub fn a() {}\n").unwrap();
            fs::write(project.path().join("group/nested/b.rs"), "pub fn b() {}\n").unwrap();
            git(project.path(), &["add", "-A"]);
            git(project.path(), &["commit", "--quiet", "-m", "subtree"]);
            publish(&request);
            if rename {
                fs::rename(project.path().join("group"), project.path().join("moved")).unwrap();
            } else {
                fs::remove_dir_all(project.path().join("group")).unwrap();
            }
            fs::write(project.path().join("f0.rs"), "pub fn notified() {}\n").unwrap();
            // No child path is reported for the removed/renamed directory.
            request.changed_paths = BTreeSet::from([b"group".to_vec(), b"f0.rs".to_vec()]);
            if rename {
                request.changed_paths.insert(b"moved".to_vec());
            }
            let manifest = publish(&request);
            assert_eq!(
                manifest.to_json_bytes().unwrap(),
                rebuild_manifest(&request).to_json_bytes().unwrap(),
                "rename={rename}"
            );
            assert!(manifest
                .entries()
                .all(|(path, _)| !path.as_bytes().starts_with(b"group/")));
        }
    }

    /// Publishes `request` and returns the served manifest, whether a new
    /// generation was published, and how many members were read from disk.
    fn publish_counting_reads(request: &AssemblyRequest) -> (Manifest, bool, usize) {
        let mut reads = 0;
        let mut count = |phase: &str| {
            reads += usize::from(phase == "working_tree_read");
            Ok(())
        };
        let mut prepared = prepare_checkout_deferring(request, true, &mut count).unwrap();
        let published = prepared.commit().unwrap().published;
        prepared.settle_status();
        drop(prepared);
        let view = ViewStore::open(&request.storage, &request.scope).unwrap();
        let manifest = view
            .load_manifest(&view.current_generation().unwrap().unwrap())
            .unwrap();
        (manifest, published, reads)
    }

    #[test]
    fn views_directory_event_reads_only_that_directorys_members() {
        let (project, _storage, mut request) = fixture();
        for directory in 0..40 {
            let path = project.path().join(format!("big/d{directory:02}"));
            fs::create_dir_all(&path).unwrap();
            for file in 0..25 {
                fs::write(
                    path.join(format!("f{file:02}.rs")),
                    format!("pub fn f{directory}_{file}() {{}}\n"),
                )
                .unwrap();
            }
        }
        fs::create_dir_all(project.path().join("edit/nested")).unwrap();
        for name in ["edit/a.rs", "edit/b.rs", "edit/nested/c.rs"] {
            fs::write(project.path().join(name), "pub fn before() {}\n").unwrap();
        }
        git(project.path(), &["add", "-A"]);
        git(project.path(), &["commit", "--quiet", "-m", "large"]);
        request.changed_paths.clear();
        publish(&request);

        // An agent's atomic write: a temporary file renamed over the member.
        // The watcher names the directory and the vanished temporary file,
        // not the member itself.
        let temporary = project.path().join("edit/.aft-source-0123.tmp");
        fs::write(&temporary, "pub fn edited() {}\n").unwrap();
        fs::rename(&temporary, project.path().join("edit/a.rs")).unwrap();
        request.changed_paths =
            BTreeSet::from([b"edit".to_vec(), b"edit/.aft-source-0123.tmp".to_vec()]);
        let (manifest, published, reads) = publish_counting_reads(&request);
        assert!(published);
        assert_eq!(reads, 3, "only the named directory's members are read");
        assert_eq!(
            manifest.to_json_bytes().unwrap(),
            rebuild_manifest(&request).to_json_bytes().unwrap()
        );

        // A deleted directory drops its members and keeps every other one.
        fs::remove_dir_all(project.path().join("edit")).unwrap();
        request.changed_paths = BTreeSet::from([b"edit".to_vec()]);
        let (manifest, published, reads) = publish_counting_reads(&request);
        assert!(published);
        assert_eq!(reads, 0);
        assert!(manifest
            .entries()
            .all(|(path, _)| !path.as_bytes().starts_with(b"edit/")));
        assert_eq!(manifest.entries().count(), 8 + 40 * 25);
        assert_eq!(
            manifest.to_json_bytes().unwrap(),
            rebuild_manifest(&request).to_json_bytes().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn views_hard_linked_nonmember_event_forces_full_read() {
        let (project, _storage, mut request) = fixture();
        fs::hard_link(
            project.path().join("f2.rs"),
            project.path().join("alias.rs"),
        )
        .unwrap();
        fs::write(
            project.path().join("alias.rs"),
            "pub fn through_alias() {}\n",
        )
        .unwrap();
        request.changed_paths = BTreeSet::from([b"alias.rs".to_vec()]);
        let (manifest, published, reads) = publish_counting_reads(&request);
        assert!(published);
        assert_eq!(reads, 8, "a hard-linked name may be any member");
        assert_eq!(
            manifest.to_json_bytes().unwrap(),
            rebuild_manifest(&request).to_json_bytes().unwrap()
        );
    }

    #[test]
    fn views_periodic_verification_reads_once_and_publishes_only_changes() {
        let (project, storage, mut request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        request.changed_paths.clear();
        expire_verification_for_test(view.view_dir());
        let (_, published, reads) = publish_counting_reads(&request);
        assert!(!published, "an unchanged checkout publishes nothing");
        assert_eq!(reads, 8, "the full read is not repeated to verify itself");
        assert!(!full_verification_due(view.view_dir()));

        // An edit no watcher event named is still found and published.
        expire_verification_for_test(view.view_dir());
        fs::write(project.path().join("f3.rs"), "pub fn missed() {}\n").unwrap();
        let (manifest, published, _) = publish_counting_reads(&request);
        assert!(published);
        assert_eq!(
            manifest.to_json_bytes().unwrap(),
            rebuild_manifest(&request).to_json_bytes().unwrap()
        );
        assert!(!full_verification_due(view.view_dir()));
    }

    #[test]
    fn views_ambiguous_watcher_spellings_force_full_read() {
        for variant in 0..3 {
            let (project, _storage, mut request) = fixture();
            let (actual, reported) = if variant == 1 {
                use unicode_normalization::UnicodeNormalization;
                let name = "caf\u{e9}.rs";
                fs::write(project.path().join(name), "pub fn before() {}\n").unwrap();
                git(project.path(), &["add", "-A"]);
                git(project.path(), &["commit", "--quiet", "-m", "unicode"]);
                publish(&request);
                let tracked = head_tree_entries(project.path())
                    .unwrap()
                    .into_iter()
                    .find(|entry| entry.rel_path.starts_with(b"caf"))
                    .unwrap();
                let spelling = String::from_utf8(tracked.rel_path).unwrap();
                let nfc: String = spelling.nfc().collect();
                let nfd: String = spelling.nfd().collect();
                (name.to_owned(), if spelling == nfc { nfd } else { nfc })
            } else {
                ("f1.rs".to_owned(), "F1.rs".to_owned())
            };
            if variant == 2 {
                fs::rename(
                    project.path().join(&actual),
                    project.path().join("case-stage.rs"),
                )
                .unwrap();
                fs::rename(
                    project.path().join("case-stage.rs"),
                    project.path().join(&reported),
                )
                .unwrap();
                fs::write(project.path().join(&reported), "pub fn case_renamed() {}\n").unwrap();
            } else {
                fs::write(project.path().join(&actual), "pub fn changed() {}\n").unwrap();
            }
            fs::write(project.path().join("f0.rs"), "pub fn notified() {}\n").unwrap();
            request.changed_paths = BTreeSet::from([b"f0.rs".to_vec(), reported.into_bytes()]);
            assert_eq!(
                publish(&request).to_json_bytes().unwrap(),
                rebuild_manifest(&request).to_json_bytes().unwrap(),
                "variant={variant}"
            );
        }
    }

    #[test]
    fn views_incremental_closure_uses_owning_connections_and_revokes_stale_proof() {
        let (_project, storage, request) = fixture();
        let view = ViewStore::open(storage.path(), &request.scope).unwrap();
        let generation = view.current_generation().unwrap().unwrap();
        let cached = previous(&view, Some(&generation)).unwrap();
        let callgraph =
            BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        let semantic =
            BlobStore::open(storage.path(), &request.family, BlobPlane::Semantic).unwrap();
        let closure = StoreClosure {
            callgraph: &callgraph,
            semantic: &semantic,
            trigram: view.trigram_path(&generation).unwrap(),
            trusted: Some((&cached.manifest, cached.stamps.clone())),
        };
        let opens = crate::db::lifecycle::thread_counts::total_opens_on_this_thread(
            crate::db::lifecycle::SqliteStore::BlobStore,
        );
        closure
            .probe_blobs(&cached.manifest.plane_keys().collect::<Vec<_>>())
            .unwrap();
        assert_eq!(
            crate::db::lifecycle::thread_counts::total_opens_on_this_thread(
                crate::db::lifecycle::SqliteStore::BlobStore
            ),
            opens
        );
        drop(closure);
        let path = callgraph.path().to_path_buf();
        drop(callgraph);
        Connection::open(path)
            .unwrap()
            .execute_batch("DELETE FROM blob_payloads")
            .unwrap();
        let callgraph =
            BlobStore::open(storage.path(), &request.family, BlobPlane::Callgraph).unwrap();
        // A base proof from before the removal must not authorize old keys.
        let stale = StoreClosure {
            callgraph: &callgraph,
            semantic: &semantic,
            trigram: view.trigram_path(&generation).unwrap(),
            trusted: Some((&cached.manifest, cached.stamps)),
        };
        assert!(matches!(
            stale.probe_blobs(&cached.manifest.plane_keys().collect::<Vec<_>>()),
            Err(ViewError::MissingBlob { .. })
        ));
    }

    #[test]
    fn views_incremental_random_sequences_equal_full_rebuild() {
        // Deterministic seeds keep a failing sequence reproducible. Each cycle
        // includes every operation, with random members, bytes and ordering.
        // Two full operation cycles per seed keep every arm in the default run;
        // nightly stress retains the longer histories and all sixteen seeds.
        let stress = std::env::var("AFT_STRESS").as_deref() == Ok("1");
        let (seeds, steps) = if stress { (16u64, 72) } else { (2, 22) };
        for seed in (1..=seeds).map(|value| value.wrapping_mul(0x9e3779b97f4a7c15)) {
            let (project, _storage, mut request) = fixture();
            let mut random = seed;
            for step in 0..steps {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let index = random as usize % 8;
                let name = format!("f{index}.rs");
                let path = project.path().join(&name);
                let mut changed = BTreeSet::from([name.as_bytes().to_vec()]);
                match (step + seed as usize) % 11 {
                    op @ (0 | 1) => {
                        let was_link = fs::symlink_metadata(&path)
                            .is_ok_and(|meta| meta.file_type().is_symlink());
                        if was_link {
                            fs::remove_file(&path).unwrap();
                        }
                        fs::write(
                            &path,
                            format!("pub fn f{index}() {{ let _x = {random}u64; }}\n"),
                        )
                        .unwrap();
                        if op == 0 || was_link {
                            git(project.path(), &["add", "-A"]);
                            git(
                                project.path(),
                                &["commit", "--quiet", "--allow-empty", "-m", "add"],
                            );
                        }
                    }
                    2 => {
                        if fs::symlink_metadata(&path).is_ok() {
                            fs::remove_file(&path).unwrap();
                        }
                    }
                    3 => {
                        let to = format!("renamed{index}.rs");
                        if fs::symlink_metadata(&path).is_ok() {
                            fs::rename(&path, project.path().join(&to)).unwrap();
                        }
                        changed.insert(to.into_bytes());
                        git(project.path(), &["add", "-A"]);
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "rename"],
                        );
                    }
                    4 => {
                        git(project.path(), &["add", "-A"]);
                        if path.is_file() {
                            let mode = if random & 1 == 0 {
                                "--chmod=+x"
                            } else {
                                "--chmod=-x"
                            };
                            git(project.path(), &["update-index", mode, &name]);
                        }
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "mode"],
                        );
                    }
                    5 => {
                        #[cfg(unix)]
                        {
                            if fs::symlink_metadata(&path).is_ok() {
                                fs::remove_file(&path).unwrap();
                            }
                            std::os::unix::fs::symlink(format!("target-{random}"), &path).unwrap();
                            git(project.path(), &["add", "-A"]);
                            git(
                                project.path(),
                                &["commit", "--quiet", "--allow-empty", "-m", "symlink"],
                            );
                        }
                    }
                    6 => {
                        git(project.path(), &["add", "-A"]);
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "head"],
                        );
                        if random & 1 == 0 {
                            git(project.path(), &["reset", "--quiet", "--hard", "HEAD~1"]);
                        }
                    }
                    8 | 9 => {
                        let directory = format!("group-{step}");
                        let nested = project.path().join(&directory).join("nested");
                        fs::create_dir_all(&nested).unwrap();
                        fs::write(
                            nested.join("child.rs"),
                            format!("pub fn child_{step}() {{}}\n"),
                        )
                        .unwrap();
                        fs::write(project.path().join("notify.rs"), "pub fn before() {}\n")
                            .unwrap();
                        git(project.path(), &["add", "-A"]);
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "directory"],
                        );
                        request.changed_paths.clear();
                        publish(&request);
                        if (step + seed as usize) % 11 == 8 {
                            fs::remove_dir_all(project.path().join(&directory)).unwrap();
                        } else {
                            let destination = format!("moved-{step}");
                            fs::rename(
                                project.path().join(&directory),
                                project.path().join(&destination),
                            )
                            .unwrap();
                            changed.insert(destination.into_bytes());
                        }
                        fs::write(
                            project.path().join("notify.rs"),
                            format!("pub fn after_{step}() {{}}\n"),
                        )
                        .unwrap();
                        changed.insert(directory.into_bytes());
                        changed.insert(b"notify.rs".to_vec());
                    }
                    10 => {
                        let lower = format!("case{step}.rs");
                        let upper = format!("CASE{step}.rs");
                        fs::write(project.path().join(&lower), "pub fn before_case() {}\n")
                            .unwrap();
                        fs::write(project.path().join("notify.rs"), "pub fn before() {}\n")
                            .unwrap();
                        git(project.path(), &["add", "-A"]);
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "case member"],
                        );
                        request.changed_paths.clear();
                        publish(&request);
                        fs::rename(
                            project.path().join(&lower),
                            project.path().join("case-stage.rs"),
                        )
                        .unwrap();
                        fs::rename(
                            project.path().join("case-stage.rs"),
                            project.path().join(&upper),
                        )
                        .unwrap();
                        fs::write(
                            project.path().join(&upper),
                            format!("pub fn case_{step}() {{}}\n"),
                        )
                        .unwrap();
                        fs::write(
                            project.path().join("notify.rs"),
                            format!("pub fn notified_{step}() {{}}\n"),
                        )
                        .unwrap();
                        changed = BTreeSet::from([upper.into_bytes(), b"notify.rs".to_vec()]);
                    }
                    _ => {
                        // A rescan must find an edit the watcher never named.
                        let rescan = project.path().join("rescan.rs");
                        fs::write(&rescan, "pub fn original() {}\n").unwrap();
                        git(project.path(), &["add", "-A"]);
                        git(
                            project.path(),
                            &["commit", "--quiet", "--allow-empty", "-m", "rescan member"],
                        );
                        request.changed_paths.clear();
                        publish(&request);
                        fs::write(&rescan, format!("pub fn rescan_{step}() {{}}\n")).unwrap();
                        changed.clear();
                    }
                }
                request.changed_paths = changed;
                let incremental = publish(&request).to_json_bytes().unwrap();
                let rebuilt = rebuild_manifest(&request).to_json_bytes().unwrap();
                assert_eq!(incremental, rebuilt, "seed={seed} step={step}");
            }
        }
    }
}
