//! Request-local root fencing for the synchronous, read-only worker surface.

use crate::{context::AppContext, protocol::Response};
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
            gate: None,
            floor_omissions: None,
            floor_filter_root: None,
        })
    };
    #[cfg(test)]
    static ASK_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The confined requests running on this thread, and the read policy their
/// directory walks check each entry against. Helper threads that do a
/// request's filesystem work carry a copy (see [`capture`] and [`inherit`]).
#[derive(Clone, Default)]
pub(crate) struct Fence {
    requests: Vec<String>,
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
    pub(crate) fn for_floor_filter_root(mut self, root: &Path) -> Self {
        self.floor_filter_root = Some(root.to_path_buf());
        self
    }

    /// The caller already matched this candidate with its discovery filters.
    pub(crate) fn record_matched_floor(&self, path: &Path) {
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
            && (crate::project_content::storage_root_for_path(path).is_some()
                || crate::db::file_identity::is_live_sqlite_file(path))
        {
            self.record_floor(path);
        }
    }

    pub(crate) fn record_matched_source_exclusion(&self, path: &Path) {
        if self.floor_omissions.is_some()
            && (crate::project_content::storage_root_for_path(path).is_some()
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

/// Carry omission reporting through the same helper-thread scope as read policy.
pub(crate) fn with_floor_disclosure(run: impl FnOnce() -> Response) -> Response {
    let omissions = Arc::new(parking_lot::Mutex::new(FloorOmissions::default()));
    let mut fence = capture();
    fence.floor_omissions = Some(omissions.clone());
    let mut response = inherit(fence, run);
    let omissions = omissions.lock();
    if !omissions.paths.is_empty() {
        response.data["skipped_files"] =
            serde_json::json!(omissions.paths.iter().map(|path| {
            serde_json::json!({"file": path.display().to_string(), "reason": "denied_floor"})
        }).collect::<Vec<_>>());
        response.data["complete"] = serde_json::json!(false);
    }
    response
}

pub(crate) fn set_floor_filter(root: &Path, filter: &crate::search_index::PathFilters) {
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
    policy: crate::access_policy::AccessPolicy,
    /// Paths the walks of this call left out because the policy denies them.
    /// A set, so a path met by two walks of one call is counted once.
    denied: parking_lot::Mutex<HashSet<PathBuf>>,
}

impl ReadGate {
    fn new(ctx: &AppContext, id: &str) -> Result<Self, Response> {
        let policy = ctx.access_policy(id)?;
        // The same root form `check_path_access` compares confined paths with.
        let root = ctx
            .config()
            .project_root
            .as_ref()
            .map(|root| {
                crate::inspect::job::normalize_path(&ctx.resolved_path_restriction_root(root))
            })
            .ok_or_else(|| refusal(id, Path::new(".")))?;
        Ok(Self {
            root,
            policy,
            denied: parking_lot::Mutex::new(HashSet::new()),
        })
    }

    fn admits(&self, path: &Path) -> bool {
        use crate::access_policy::{AccessDecision, AccessOperation};
        let identity = crate::access_policy::policy_path(path);
        identity.starts_with(&self.root)
            && self.policy.decide(AccessOperation::Read, &identity, false) == AccessDecision::Allow
    }
}

#[cfg(test)]
pub(crate) fn note_ask() {
    ASK_CALLS.with(|calls| calls.set(calls.get() + 1));
}

#[cfg(all(test, unix))]
pub(crate) fn ask_calls() -> usize {
    ASK_CALLS.with(|calls| calls.get())
}

pub(crate) fn active() -> bool {
    FENCE.with(|fence| fence.borrow().active())
}

pub(crate) fn for_request(id: &str) -> bool {
    FENCE.with(|fence| fence.borrow().requests.iter().any(|request| request == id))
}

pub(crate) fn capture() -> Fence {
    FENCE.with(|fence| fence.borrow().clone())
}

pub(crate) fn inherit<R>(fence: Fence, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<Fence>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                FENCE.with(|fence| *fence.borrow_mut() = previous);
            }
        }
    }
    let _restore = Restore(Some(FENCE.with(|current| current.replace(fence))));
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
pub(crate) fn disclose_denied(body: &mut Value) {
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
pub(crate) fn admits_walk_root(root: &Path) -> bool {
    FENCE.with(|fence| {
        let fence = fence.borrow();
        // A confined thread without its read policy fails closed.
        !fence.active() || fence.gate.as_ref().is_some_and(|gate| gate.admits(root))
    })
}

/// [`admits_walk_root`] as a request result: a refused root gets the same
/// refusal the confined path check gives.
pub(crate) fn check_walk_root(id: &str, root: &Path) -> Result<(), Response> {
    if admits_walk_root(root) {
        Ok(())
    } else {
        Err(refusal(id, root))
    }
}

pub(crate) fn refusal(id: &str, input: &Path) -> Response {
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
    ctx: &AppContext,
    id: &str,
    run: impl FnOnce() -> Result<R, Response>,
) -> Result<R, Response> {
    struct Restore(Option<Fence>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                FENCE.with(|fence| *fence.borrow_mut() = previous);
            }
        }
    }
    let _restore = Restore(Some(FENCE.with(|fence| {
        let mut fence = fence.borrow_mut();
        let previous = fence.clone();
        fence.requests.push(id.to_owned());
        previous
    })));
    ctx.with_force_restrict(id, || {
        let gate = Arc::new(ReadGate::new(ctx, id)?);
        FENCE.with(|fence| fence.borrow_mut().gate = Some(gate));
        run()
    })
}

/// Validate original spellings before translation can join them onto a root or
/// reinterpret an absent file as a repository selector or remote resource.
pub(crate) fn preflight(
    ctx: &AppContext,
    id: &str,
    session: &str,
    args: &Value,
) -> Result<(), Response> {
    fn validate(ctx: &AppContext, id: &str, session: &str, value: &Value) -> Result<(), Response> {
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
pub(crate) struct ConfinedIgnoreRules {
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
pub(crate) const SEARCH_IGNORE_FILES: &[&str] = &[".aftignore", ".ignore", ".gitignore"];
/// The ignore families a confined outline directory walk honors.
pub(crate) const OUTLINE_IGNORE_FILES: &[&str] = &[".ignore", ".gitignore"];
/// The ignore family a confined outline files-mode walk honors.
pub(crate) const OUTLINE_FILES_IGNORE_FILES: &[&str] = &[".gitignore"];

/// Largest ignore file a confined walk reads; a larger one is not applied.
const MAX_IGNORE_FILE_BYTES: u64 = 1024 * 1024;

impl ConfinedIgnoreRules {
    /// Rules for a walk on this thread, or `None` when no confined call runs.
    pub(crate) fn current(names: &'static [&'static str]) -> Option<Arc<Self>> {
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
    pub(crate) fn admits(&self, path: &Path) -> bool {
        self.fence.admits_walk_entry(path)
    }

    /// Whether the confined root's own ignore files exclude `path`.
    pub(crate) fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
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
        let ignore_root = ignore_root.map(crate::inspect::job::normalize_path);
        let root = ignore_root
            .as_deref()
            .filter(|ignore_root| ignore_root.starts_with(root))
            .unwrap_or(root);
        // The gate's root has the Windows verbatim prefix (`\\?\C:\...`)
        // stripped, but a walk started from a canonicalized path yields entries
        // that keep it. Compare, load and match in the root's spelling, or no
        // directory would count as inside the root and no rule would apply.
        // Only this local copy changes; the walk's paths stay as they are.
        let path = crate::inspect::job::normalize_path(path);
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
