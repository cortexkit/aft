//! Request-local root fencing for the synchronous, read-only worker surface.

use crate::{protocol::Response, read_policy::ReadPolicy};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

thread_local! {
    static FENCE: RefCell<Fence> = const {
        RefCell::new(Fence {
            requests: Vec::new(),
            remote_mutation: false,
            gate: None,
            floor_omissions: None,
            floor_filter_root: None,
        })
    };
    #[cfg(any(test, all(unix, feature = "test-support")))]
    static ASK_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The confined requests running on this thread, and the read policy their
/// directory walks check each entry against. Helper threads that do a
/// request's filesystem work carry a copy (see [`capture`] and [`inherit`]).
#[derive(Clone, Default)]
#[doc(hidden)]
pub struct Fence {
    requests: Vec<String>,
    remote_mutation: bool,
    gate: Option<Arc<ReadGate>>,
    floor_omissions: Option<Arc<parking_lot::Mutex<FloorOmissions>>>,
    floor_filter_root: Option<PathBuf>,
}

#[derive(Default)]
struct FloorOmissions {
    paths: BTreeSet<PathBuf>,
    filter: Option<(PathBuf, crate::search_index::PathFilters)>,
}

impl PartialEq for Fence {
    fn eq(&self, other: &Self) -> bool {
        self.requests == other.requests
            && self.remote_mutation == other.remote_mutation
            && self.floor_filter_root == other.floor_filter_root
            && match (&self.floor_omissions, &other.floor_omissions) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
            && match (&self.gate, &other.gate) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
    }
}

impl std::fmt::Debug for Fence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fence")
            .field("requests", &self.requests)
            .field("read_gate", &self.gate.is_some())
            .finish()
    }
}

impl Fence {
    /// Optional background work may outlive its reply. Keep its read authority,
    /// but do not let it append omissions to a response that is already sent.
    pub(crate) fn without_floor_disclosure(mut self) -> Self {
        self.floor_omissions = None;
        self
    }

    pub(crate) fn for_floor_filter_root(mut self, root: &Path) -> Self {
        self.floor_filter_root = Some(root.to_path_buf());
        self
    }

    /// The caller already matched this candidate with its discovery filters.
    #[doc(hidden)]
    pub fn record_matched_floor(&self, path: &Path) {
        if let Some(omissions) = &self.floor_omissions {
            omissions.lock().paths.insert(path.to_path_buf());
        }
    }

    pub(crate) fn record_floor(&self, path: &Path) {
        if let Some(omissions) = &self.floor_omissions {
            let is_dir = path.is_dir();
            let mut omissions = omissions.lock();
            if is_dir
                || omissions.filter.as_ref().is_none_or(|(root, filter)| {
                    filter.matches(self.floor_filter_root.as_deref().unwrap_or(root), path)
                })
            {
                omissions.paths.insert(path.to_path_buf());
            }
        }
    }

    pub(crate) fn record_source_exclusion(&self, path: &Path) {
        if self.floor_omissions.is_some()
            && (crate::project_content::storage_root_for_path(
                path,
                &crate::access_policy::StoragePaths,
            )
            .is_some()
                || crate::db::file_identity::is_live_sqlite_file(path))
        {
            self.record_floor(path);
        }
    }

    pub(crate) fn record_matched_source_exclusion(&self, path: &Path) {
        if self.floor_omissions.is_some()
            && (crate::project_content::storage_root_for_path(
                path,
                &crate::access_policy::StoragePaths,
            )
            .is_some()
                || crate::db::file_identity::is_live_sqlite_file(path))
        {
            self.record_matched_floor(path);
        }
    }

    fn active(&self) -> bool {
        !self.requests.is_empty()
    }

    /// Whether a confined walk may yield `path` (a file, or a directory to
    /// descend into). The read policy is the one `read` applies to a single
    /// path: inside the confined root and not denied by a floor such as
    /// `sandbox.read_deny`. A denied path is remembered so the reply can say
    /// how many paths were left out. Unconfined walks admit everything.
    pub(crate) fn admits_walk_entry(&self, path: &Path) -> bool {
        if !self.active() {
            return true;
        }
        // A confined thread without its read policy fails closed.
        let Some(gate) = &self.gate else {
            return false;
        };
        if gate.admits(path) {
            return true;
        }
        gate.denied.lock().insert(path.to_path_buf());
        false
    }
}

#[cfg(test)]
mod ignore_probe_tests {
    use super::*;

    #[test]
    fn grep_ignore_disclosure_detached_fence_keeps_authority_not_output_collector() {
        let dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(ReadGate::new(dir.path().to_path_buf(), Arc::new(|_| true)));
        let omissions = Arc::new(parking_lot::Mutex::new(FloorOmissions::default()));
        let original = Fence {
            requests: vec!["probe-fence".to_string()],
            remote_mutation: false,
            gate: Some(Arc::clone(&gate)),
            floor_omissions: Some(Arc::clone(&omissions)),
            floor_filter_root: None,
        };
        let detached = original.clone().without_floor_disclosure();
        assert_eq!(detached.requests, original.requests);
        assert!(Arc::ptr_eq(detached.gate.as_ref().unwrap(), &gate));
        detached.record_matched_floor(&dir.path().join("not-reported"));
        assert!(omissions.lock().paths.is_empty());
        original.record_matched_floor(&dir.path().join("still-reported"));
        assert_eq!(omissions.lock().paths.len(), 1);
    }
}

/// Carry omission reporting through the same helper-thread scope as read policy.
#[doc(hidden)]
pub fn with_floor_disclosure(run: impl FnOnce() -> Response) -> Response {
    let omissions = Arc::new(parking_lot::Mutex::new(FloorOmissions::default()));
    let mut fence = capture();
    fence.floor_omissions = Some(omissions.clone());
    let mut response = inherit(fence, run);
    let omissions = omissions.lock();
    if !omissions.paths.is_empty() {
        if crate::bounded_io::skip_disclosure_active() {
            for path in &omissions.paths {
                crate::bounded_io::note_skipped_file(path, "denied_floor");
            }
        } else {
            // Other commands own their skip order, completeness and rendering.
            // Preserve their uncapped floor report: search/grep's 20-name cap
            // and result-list trailer do not belong on these response surfaces.
            response.data["skipped_files"] = serde_json::json!(omissions.paths.iter().map(|path| serde_json::json!({"file":path.display().to_string(),"reason":"denied_floor"})).collect::<Vec<_>>());
            response.data["complete"] = serde_json::json!(false);
        }
    }
    response
}

#[doc(hidden)]
pub fn set_floor_filter(root: &Path, filter: &crate::search_index::PathFilters) {
    FENCE.with(|fence| {
        if let Some(omissions) = &fence.borrow().floor_omissions {
            omissions.lock().filter = Some((root.to_path_buf(), filter.clone()));
        }
    });
}

/// The read policy one confined call checks walked paths against, snapshotted
/// once per call so a walk does not rebuild it for every entry.
pub(crate) struct ReadGate {
    root: PathBuf,
    decision: Arc<dyn Fn(&Path) -> bool + Send + Sync>,
    /// Paths the walks of this call left out because the policy denies them.
    /// A set, so a path met by two walks of one call is counted once.
    denied: parking_lot::Mutex<HashSet<PathBuf>>,
}

impl ReadGate {
    pub(crate) fn new(root: PathBuf, decision: Arc<dyn Fn(&Path) -> bool + Send + Sync>) -> Self {
        Self {
            root,
            decision,
            denied: parking_lot::Mutex::new(HashSet::new()),
        }
    }

    fn admits(&self, path: &Path) -> bool {
        (self.decision)(path)
    }
}

pub(crate) fn with_gate<R>(gate: ReadGate, run: impl FnOnce() -> R) -> R {
    // Gate assignment deliberately outlives this call, until the enclosing fence is restored.
    let mut fence = capture();
    fence.gate = Some(Arc::new(gate));
    replace_fence(fence);
    run()
}

#[cfg(any(test, all(unix, feature = "test-support")))]
pub(crate) fn note_ask() {
    ASK_CALLS.with(|calls| calls.set(calls.get() + 1));
}

#[cfg(all(any(test, feature = "test-support"), unix))]
pub(crate) fn ask_calls() -> usize {
    ASK_CALLS.with(|calls| calls.get())
}

pub fn active() -> bool {
    FENCE.with(|fence| fence.borrow().active())
}

#[doc(hidden)]
pub fn remote_mutation() -> bool {
    FENCE.with(|fence| fence.borrow().remote_mutation)
}

pub(crate) fn admits_path_metadata(identity: &Path) -> bool {
    match crate::bounded_io::metadata(identity) {
        Ok(_) => true,
        // Remote edit tools can create files after the root and access checks.
        // For those calls, only a missing location is not an I/O refusal.
        Err(error) => remote_mutation() && error.kind() == std::io::ErrorKind::NotFound,
    }
}

#[doc(hidden)]
pub fn with_remote_mutation<T>(mutation: bool, run: impl FnOnce() -> T) -> T {
    if mutation {
        let mut fence = capture();
        fence.remote_mutation = true;
        inherit(fence, run)
    } else {
        run()
    }
}

pub(crate) fn for_request(id: &str) -> bool {
    FENCE.with(|fence| fence.borrow().requests.iter().any(|request| request == id))
}

#[doc(hidden)]
pub fn capture() -> Fence {
    FENCE.with(|fence| fence.borrow().clone())
}

struct FenceScope {
    previous: Option<Fence>,
}

/// Keep the opaque helper carrier identical to the typed thread-local fence.
/// A standalone gate remains installed even without an active request.
fn replace_fence(fence: Fence) -> Fence {
    let factory: Option<crate::bounded_io::CarryFactory> = if fence == Fence::default() {
        None
    } else {
        let carried = fence.clone();
        Some(Arc::new(move || {
            let fence = carried.clone();
            Ok(Box::new(move || Ok(Box::new(FenceScope::enter(fence)))))
        }))
    };
    let previous = FENCE.with(|current| current.replace(fence));
    let restore = previous.clone();
    crate::bounded_io::replace_carry_factory(
        factory,
        Box::new(move || {
            replace_fence(restore);
        }),
    );
    previous
}

impl FenceScope {
    fn enter(fence: Fence) -> Self {
        Self {
            previous: Some(replace_fence(fence)),
        }
    }
}

impl Drop for FenceScope {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            replace_fence(previous);
        }
    }
}

pub(crate) fn inherit<R>(fence: Fence, run: impl FnOnce() -> R) -> R {
    let _scope = FenceScope::enter(fence);
    run()
}

/// How many paths this thread's confined call has left out of its walks
/// because the read policy denies them.
pub(crate) fn denied_count() -> usize {
    FENCE.with(|fence| {
        fence
            .borrow()
            .gate
            .as_ref()
            .map_or(0, |gate| gate.denied.lock().len())
    })
}

/// Say in a walk's reply how many paths the read policy kept out of it, so a
/// skipped file is never silent: `skipped_denied` carries the count and the
/// text gains a note. Only the count is reported; a denied path's name is not.
#[doc(hidden)]
pub fn disclose_denied(body: &mut Value) {
    let count = denied_count();
    if count == 0 {
        return;
    }
    body["skipped_denied"] = serde_json::json!(count);
    let note = format!(
        "({count} path(s) under the walked directory are denied by the read policy and were skipped.)"
    );
    let text = body["text"].as_str().unwrap_or_default();
    // A directory outline ends its text with a structure footer that the
    // formatter peels off by suffix, so the note goes before that footer.
    let footer = body
        .get("structure_footer")
        .and_then(Value::as_str)
        .filter(|footer| !footer.is_empty());
    let split = footer.and_then(|footer| {
        text.strip_suffix(&format!("\n{footer}"))
            .map(|head| (head, footer))
    });
    let text = match split {
        Some((head, footer)) => format!("{}\n\n{note}\n{footer}", head.trim_end_matches('\n')),
        None if text.is_empty() => note,
        None => format!("{}\n\n{note}", text.trim_end_matches('\n')),
    };
    body["text"] = Value::String(text);
}

/// Whether a walk on this thread may start at `root`. A confined walk checks
/// its starting path against the same gate as every entry under it: inside
/// the confined root and admitted by the read policy. The request's earlier
/// path check normally refuses such a root first; this keeps the walk itself
/// from starting outside the fence when a caller skipped that check.
/// Unconfined walks always may start. A refused root is not counted among the
/// call's skipped entries, because the call is refused rather than trimmed.
#[doc(hidden)]
pub fn admits_walk_root(root: &Path) -> bool {
    FENCE.with(|fence| {
        let fence = fence.borrow();
        // A confined thread without its read policy fails closed.
        !fence.active() || fence.gate.as_ref().is_some_and(|gate| gate.admits(root))
    })
}

/// [`admits_walk_root`] as a request result: a refused root gets the same
/// refusal the confined path check gives.
#[doc(hidden)]
pub fn check_walk_root(id: &str, root: &Path) -> Result<(), Response> {
    if admits_walk_root(root) {
        Ok(())
    } else {
        Err(refusal(id, root))
    }
}

#[doc(hidden)]
pub fn refusal(id: &str, input: &Path) -> Response {
    Response::error(
        id,
        "path_outside_confined_root",
        format!(
            "path '{}' is not available in this confined root",
            input.display()
        ),
    )
}

/// Run `run` as confined request `id`: path checks for `id` refuse anything
/// outside the root, and directory walks on this thread (and on helper
/// threads that inherit the fence) check each entry against the call's read
/// policy, built here once.
pub(crate) fn with_root<R>(
    id: &str,
    run: impl FnOnce() -> Result<R, Response>,
) -> Result<R, Response> {
    let mut fence = capture();
    fence.requests.push(id.to_owned());
    let _scope = FenceScope::enter(fence);
    run()
}

/// Validate original spellings before translation can join them onto a root or
/// reinterpret an absent file as a repository selector or remote resource.
#[doc(hidden)]
pub fn preflight(
    ctx: &dyn ReadPolicy,
    id: &str,
    session: &str,
    args: &Value,
) -> Result<(), Response> {
    fn validate(
        ctx: &dyn ReadPolicy,
        id: &str,
        session: &str,
        value: &Value,
    ) -> Result<(), Response> {
        if let Some(input) = value.as_str() {
            if input.contains("://") {
                return Err(refusal(id, Path::new(input)));
            }
            ctx.validate_read_path(id, session, Path::new(input))?;
        } else if let Some(items) = value.as_array() {
            for item in items {
                validate(ctx, id, session, item)?;
            }
        }
        Ok(())
    }
    for key in ["path", "filePath", "target", "directory", "files", "url"] {
        if let Some(value) = args.get(key) {
            validate(ctx, id, session, value)?;
        }
    }
    ctx.validate_read_path(id, session, Path::new("."))?;
    Ok(())
}

/// Ignore rules for a confined walk, read only from ignore files the confined
/// root itself holds.
///
/// An unconfined walk lets the `ignore` crate load rules from global excludes,
/// `.git/info/exclude`, parent directories above the root, and ignore files
/// that are symlinks to anywhere. A confined walk turns all of that off (see
/// `apply_project_ignore_rules`) and filters entries through this instead: in
/// each directory from the root down, an ignore file counts only when it is a
/// regular file (never a symlink) that the call's read policy admits, and it
/// is opened without following links.
///
/// `names` lists the ignore file names the unconfined walk honors, highest
/// precedence first. As in the `ignore` crate, the first family with a
/// matching rule decides, and within a family the deepest directory's rule
/// wins; `.gitignore` rules stop at the nearest directory holding `.git`.
#[doc(hidden)]
pub struct ConfinedIgnoreRules {
    fence: Fence,
    names: &'static [&'static str],
    directories: parking_lot::Mutex<HashMap<PathBuf, Arc<DirectoryRules>>>,
}

struct DirectoryRules {
    /// One matcher per entry of [`ConfinedIgnoreRules::names`].
    matchers: Vec<Option<Gitignore>>,
    has_git: bool,
}

/// The ignore families a confined grep or glob walk honors.
#[doc(hidden)]
pub const SEARCH_IGNORE_FILES: &[&str] = &[".aftignore", ".ignore", ".gitignore"];
/// The ignore families a confined outline directory walk honors.
#[doc(hidden)]
pub const OUTLINE_IGNORE_FILES: &[&str] = &[".ignore", ".gitignore"];
/// The ignore family a confined outline files-mode walk honors.
#[doc(hidden)]
pub const OUTLINE_FILES_IGNORE_FILES: &[&str] = &[".gitignore"];

/// Largest ignore file a confined walk reads; a larger one is not applied.
const MAX_IGNORE_FILE_BYTES: u64 = 1024 * 1024;

impl ConfinedIgnoreRules {
    /// Rules for a walk on this thread, or `None` when no confined call runs.
    #[doc(hidden)]
    pub fn current(names: &'static [&'static str]) -> Option<Arc<Self>> {
        let fence = capture();
        fence.active().then(|| {
            Arc::new(Self {
                fence,
                names,
                directories: parking_lot::Mutex::new(HashMap::new()),
            })
        })
    }

    /// The read policy check for a walked entry, recording a denial; see
    /// [`Fence::admits_walk_entry`].
    #[doc(hidden)]
    pub fn admits(&self, path: &Path) -> bool {
        self.fence.admits_walk_entry(path)
    }

    /// Whether the confined root's own ignore files exclude `path`.
    #[doc(hidden)]
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        self.is_ignored_with_root(path, is_dir, None)
    }

    /// Ignore files above a folder explicitly named in a glob do not apply,
    /// but all file reads still use the original confinement gate.
    pub(crate) fn is_ignored_below(&self, path: &Path, is_dir: bool, root: &Path) -> bool {
        self.is_ignored_with_root(path, is_dir, Some(root))
    }

    fn is_ignored_with_root(&self, path: &Path, is_dir: bool, ignore_root: Option<&Path>) -> bool {
        let Some(root) = self.fence.gate.as_ref().map(|gate| gate.root.as_path()) else {
            return false;
        };
        let ignore_root = ignore_root.map(crate::path_identity::normalize_path);
        let root = ignore_root
            .as_deref()
            .filter(|ignore_root| ignore_root.starts_with(root))
            .unwrap_or(root);
        // The gate's root has the Windows verbatim prefix (`\\?\C:\...`)
        // stripped, but a walk started from a canonicalized path yields entries
        // that keep it. Compare, load and match in the root's spelling, or no
        // directory would count as inside the root and no rule would apply.
        // Only this local copy changes; the walk's paths stay as they are.
        let path = crate::path_identity::normalize_path(path);
        let path = path.as_path();
        let Some(parent) = path.parent() else {
            return false;
        };
        let chain = parent
            .ancestors()
            .take_while(|directory| directory.starts_with(root))
            .map(|directory| self.directory(directory))
            .collect::<Vec<_>>();
        for (index, name) in self.names.iter().enumerate() {
            for rules in &chain {
                if let Some(matcher) = &rules.matchers[index] {
                    let matched = matcher.matched(path, is_dir);
                    if matched.is_ignore() {
                        return true;
                    }
                    if matched.is_whitelist() {
                        return false;
                    }
                }
                if *name == ".gitignore" && rules.has_git {
                    break;
                }
            }
        }
        false
    }

    fn directory(&self, directory: &Path) -> Arc<DirectoryRules> {
        if let Some(rules) = self.directories.lock().get(directory) {
            return Arc::clone(rules);
        }
        let rules = Arc::new(DirectoryRules {
            matchers: self
                .names
                .iter()
                .map(|name| self.load(directory, name))
                .collect(),
            has_git: std::fs::symlink_metadata(directory.join(".git")).is_ok(),
        });
        self.directories
            .lock()
            .insert(directory.to_path_buf(), Arc::clone(&rules));
        rules
    }

    fn load(&self, directory: &Path, name: &str) -> Option<Gitignore> {
        let file = directory.join(name);
        let metadata = std::fs::symlink_metadata(&file).ok()?;
        let admitted = self
            .fence
            .gate
            .as_ref()
            .is_some_and(|gate| gate.admits(&file));
        if !metadata.file_type().is_file() || !admitted {
            return None;
        }
        let text = String::from_utf8(read_without_following(&file).ok()?).ok()?;
        let mut builder = GitignoreBuilder::new(directory);
        for line in text.lines() {
            // A malformed line is skipped, as the `ignore` crate does.
            let _ = builder.add_line(Some(file.clone()), line);
        }
        builder.build().ok().filter(|matcher| !matcher.is_empty())
    }
}

/// Read a regular file without following a symlink at its final component:
/// a file swapped for a link after it was checked is refused, not followed.
fn read_without_following(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    #[cfg(all(test, unix))]
    crate::bounded_io::tests::note_open(path);
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open a link itself, never its target.
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_IGNORE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_IGNORE_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ignore file too large",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn assert_helper_fence() {
        let expected = capture();
        let expected_denied = denied_count();
        let expected_carrier = expected != Fence::default();
        assert_eq!(crate::bounded_io::has_carry_factory(), expected_carrier);
        let pool = Arc::new(crate::bounded_io::IsolatedPool::new(2));
        let nested_pool = pool.clone();
        let caller_thread = std::thread::current().id();
        let (helper, nested, denied, carried, helper_thread, nested_thread) = pool
            .run(Path::new("."), None, move |_| {
                let helper = capture();
                let helper_thread = std::thread::current().id();
                let nested = nested_pool.run(
                    Path::new("."),
                    Some(Instant::now() + Duration::from_secs(1)),
                    |_| Ok((capture(), std::thread::current().id())),
                )?;
                Ok((
                    helper,
                    nested.0,
                    denied_count(),
                    crate::bounded_io::has_carry_factory(),
                    helper_thread,
                    nested.1,
                ))
            })
            .unwrap();
        assert_ne!(caller_thread, helper_thread);
        assert_ne!(
            helper_thread, nested_thread,
            "nested work must dispatch another helper"
        );
        assert_eq!(helper, expected);
        assert_eq!(nested, expected);
        assert_eq!(denied, expected_denied);
        assert_eq!(carried, expected_carrier);
    }

    fn gate() -> ReadGate {
        let gate = ReadGate::new(PathBuf::from("root"), Arc::new(|_| false));
        gate.denied.lock().insert(PathBuf::from("denied"));
        gate
    }

    #[test]
    fn default_fence_has_no_carrier_on_fresh_caller_or_helpers() {
        std::thread::spawn(|| {
            assert_eq!(capture(), Fence::default());
            assert_helper_fence();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn every_fence_entry_carries_exact_state_to_nested_helpers() {
        std::thread::spawn(|| {
            with_root("outer", || {
                assert_helper_fence();
                with_gate(gate(), assert_helper_fence);
                assert!(
                    capture().gate.is_some(),
                    "gate assignment must outlive with_gate"
                );
                assert_helper_fence();
                let fence = capture().for_floor_filter_root(Path::new("filter-root"));
                inherit(fence, assert_helper_fence);
                assert_helper_fence();
                with_floor_disclosure(|| {
                    assert_helper_fence();
                    Response::error("test", "test", "test")
                });
                assert_helper_fence();
                Ok(())
            })
            .unwrap();
            assert_eq!(capture(), Fence::default());
            assert_helper_fence();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn standalone_gate_and_denied_count_are_carried_after_with_gate() {
        std::thread::spawn(|| {
            with_gate(gate(), assert_helper_fence);
            assert!(!active());
            assert!(capture().gate.is_some());
            assert_eq!(denied_count(), 1);
            assert_helper_fence();
            inherit(Fence::default(), assert_helper_fence);
            assert_eq!(denied_count(), 1);
            assert_helper_fence();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn helper_local_standalone_gate_cannot_leak_to_the_next_unconfined_job() {
        std::thread::spawn(|| {
            let pool = crate::bounded_io::IsolatedPool::new(1);
            let first = pool
                .run(Path::new("."), None, |_| {
                    with_gate(gate(), || {});
                    assert!(capture().gate.is_some());
                    Ok(std::thread::current().id())
                })
                .unwrap();
            let second = pool
                .run(Path::new("."), None, |_| {
                    assert_eq!(capture(), Fence::default());
                    assert!(!crate::bounded_io::has_carry_factory());
                    Ok(std::thread::current().id())
                })
                .unwrap();
            assert_eq!(first, second, "the jobs must reuse the same helper");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn fence_and_carrier_restore_together_after_unwind() {
        std::thread::spawn(|| {
            let unwound = std::panic::catch_unwind(|| {
                with_root::<()>("outer", || {
                    with_gate(gate(), || {
                        inherit(capture(), || panic!("leave inherited fence"))
                    })
                })
                .unwrap();
            });
            assert!(unwound.is_err());
            assert_eq!(capture(), Fence::default());
            assert_helper_fence();
        })
        .join()
        .unwrap();
    }
}
