//! Bounded, observational shell-change reporting. No tree walk or file snapshots.
use crate::config::BashChangedFiles;
use crate::list_envelope::{ListEnvelope, Reason};
use serde::Serialize;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) const MAX_EVENTS: usize = 8192;
pub(crate) const MAX_FILES: usize = 40;
pub(crate) const MAX_DIFF_LINES: usize = 200;
pub(crate) const MAX_REPORT_BYTES: usize = 64 * 1024;
const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_RENDER_TIME: Duration = Duration::from_secs(2);
// Shared by both Git queries, including process startup and pipe collection.
const GIT_CAPTURE_LIMIT: Duration = Duration::from_secs(1);

#[cfg(test)]
thread_local! {
    /// Lets a correctness test give the git before-image capture an ample
    /// budget on its own thread, so a loaded runner's slow git startup cannot
    /// turn the result into the over-budget path the test is not about.
    pub(crate) static TEST_GIT_CAPTURE_BUDGET: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}
const HEADER: &str =
    "Files changed while the command ran (changes by other processes may be included):";
const DIRTY: &str = "no diff: it had uncommitted changes before the command";
const OUTSIDE_GIT: &str =
    "No diffs: the project is not in a git repository, so only paths are listed.";
const GIT_UNAVAILABLE: &str = "No diffs: git before-images could not be captured within the time/output budget, so only paths are listed.";
const OVERFLOW: &str = "Incomplete: the file watcher overflowed, so some changes may be missing.";
pub(crate) const NOT_CAUGHT_UP: &str = "Incomplete: the file watcher had not caught up when the command ended, so some changes may be missing.";
const NOT_SHOWN: &str = "not shown: file is denied or could not be safely opened";
// Git answering "no such path" and Git being killed at the deadline look the
// same to the caller, so a missing answer after the deadline must never be
// reported as a missing before-image.
const NO_BEFORE_IMAGE: &str =
    "no diff: no clean git before-image is available (untracked, unreadable or too large)";
const DIFF_BUDGET_SPENT: &str = "no diff: the diff time budget ran out";

fn comparison_path(path: &Path) -> Cow<'_, Path> {
    // Git and canonicalize can spell the same Windows path differently.
    // Use one spelling for comparisons and Git, without rewriting file I/O paths.
    #[cfg(windows)]
    {
        Cow::Owned(crate::windows_path::normalize_windows_path(path))
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

fn git_comparison_path(path: &Path) -> PathBuf {
    // Git and the watcher can use different filesystem aliases for the same
    // path. Resolve comparison copies, including missing leaves, without
    // changing stored paths or the descriptor checks used to authorize reads.
    let resolved = crate::context::canonicalize_lenient(path).unwrap_or_else(|| path.to_path_buf());
    comparison_path(&resolved).into_owned()
}

fn relative_path_text(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

#[derive(Default, Debug)]
pub(crate) struct Journal {
    state: Mutex<JournalState>,
}
#[derive(Default, Debug)]
struct JournalState {
    sequence: u64,
    gaps: u64,
    unreported_renames: u64,
    events: VecDeque<(u64, PathBuf, bool, bool)>,
}
impl Journal {
    /// Raw watcher intake precedes batch filtering, so reports must apply the
    /// same exclusions before retaining paths in their own event history.
    pub(crate) fn event_for_root(
        &self,
        event: &notify::Event,
        root: &Path,
        matcher: &crate::watcher_filter::SharedGitignore,
    ) {
        let mut admitted = event.clone();
        admitted.paths.retain(|path| {
            let directory = matches!(
                event.kind,
                notify::EventKind::Create(notify::event::CreateKind::Folder)
                    | notify::EventKind::Remove(notify::event::RemoveKind::Folder)
            ) || path.is_dir();
            !crate::watcher_filter::watcher_path_is_ignored_by_matcher_with_kind(
                matcher, path, root, directory,
            )
        });
        self.event(&admitted);
    }
    pub(crate) fn event(&self, event: &notify::Event) {
        if !crate::watcher_filter::watcher_event_invalidates(&event.kind) {
            return;
        }
        if matches!(
            event.kind,
            notify::EventKind::Create(notify::event::CreateKind::Folder)
                | notify::EventKind::Remove(notify::event::RemoveKind::Folder)
        ) {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rename = matches!(
            event.kind,
            notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
        );
        for path in &event.paths {
            let directory = path.is_dir();
            if rename && (directory || !path.exists()) {
                // A vanished rename path is not proof of a deleted file. The
                // watcher may have reported only the directory, not its children.
                state.unreported_renames += 1;
                if !directory {
                    continue;
                }
            }
            state.sequence += 1;
            let sequence = state.sequence;
            if state.events.len() == MAX_EVENTS {
                state.events.pop_front();
            }
            state
                .events
                .push_back((sequence, path.clone(), event.kind.is_create(), directory));
        }
    }
    pub(crate) fn gap(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .gaps += 1;
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_lines_list_envelope: Option<ListEnvelope>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub text: String,
    pub files: Vec<ChangedFile>,
    pub incomplete: Vec<String>,
    pub changed_files_list_envelope: ListEnvelope,
}

fn render_file(file: &ChangedFile) -> String {
    let path = if file.path.chars().any(char::is_control) {
        serde_json::to_string(&file.path).unwrap_or_default()
    } else {
        file.path.clone()
    };
    let mut rendered = format!("\n  {} {}", file.status, path);
    if let Some(reason) = &file.reason {
        rendered.push_str(&format!(" ({reason})"));
    }
    if let Some(diff) = &file.diff {
        rendered.push('\n');
        rendered.push_str(diff);
    }
    rendered
}

fn report_body_bytes(files: &[ChangedFile]) -> usize {
    let mut text = HEADER.to_string();
    for file in files {
        text.push_str(&render_file(file));
    }
    serde_json::to_vec(&text).map_or(MAX_REPORT_BYTES, |bytes| bytes.len())
        + serde_json::to_vec(files).map_or(MAX_REPORT_BYTES, |bytes| bytes.len())
}

#[derive(Debug)]
pub(crate) struct Capture {
    root: PathBuf,
    journal: Arc<Journal>,
    sequence: u64,
    gaps: u64,
    unreported_renames: u64,
    mode: BashChangedFiles,
    watcher_active: bool,
    flush: Option<Arc<crate::watcher_backend::flush::FlushControl>>,
    call_deadline: SharedCallDeadline,
    git: Option<GitStart>,
    read_policy: Option<ReadPolicy>,
    end: OnceLock<(u64, u64, u64)>,
    report: OnceLock<Report>,
    #[cfg(test)]
    diff_delay_for_test: Duration,
    /// Replaces `MAX_RENDER_TIME` so tests that check report content are not
    /// at the mercy of Git process startup time on a loaded runner. Tests of
    /// deadline behaviour set an explicit call deadline instead.
    #[cfg(test)]
    render_time_for_test: Duration,
}
#[derive(Debug)]
struct GitStart {
    directory: PathBuf,
    head: String,
    dirty: BTreeSet<PathBuf>,
}

#[derive(Debug)]
struct ReadPolicy {
    access: crate::access_policy::AccessPolicy,
    deny_prefixes: Vec<PathBuf>,
    native_data: Option<crate::sandbox_profile::SandboxDataPolicy>,
}

impl ReadPolicy {
    fn new(
        mut access: crate::access_policy::AccessPolicy,
        profile: Option<&crate::sandbox_profile::SandboxProfile>,
    ) -> Self {
        let mut deny_prefixes = Vec::new();
        if let Some(profile) = profile {
            // Use the actual launch profile so reports preserve its hook,
            // credential and environment read restrictions.
            access.absolute_floor.extend(
                profile
                    .read_deny
                    .iter()
                    .chain(&profile.keychain_deny)
                    .cloned(),
            );
            deny_prefixes.extend(profile.deny_prefixes.iter().cloned());
        }
        Self {
            access,
            deny_prefixes,
            native_data: profile.map(|profile| profile.data_policy.clone()),
        }
    }

    fn allows(&self, path: &Path) -> bool {
        // Compare normalized copies: Windows canonicalization can add a path
        // prefix that is absent from watcher paths. Keep launch paths unchanged.
        let path = crate::inspect::job::normalize_path(path);
        self.access
            .decide(crate::access_policy::AccessOperation::Read, &path, false)
            == crate::access_policy::AccessDecision::Allow
            && self.native_data.as_ref().is_none_or(|data| {
                !data
                    .deny
                    .iter()
                    .any(|denied| path.starts_with(crate::inspect::job::normalize_path(denied)))
                    || data.read_allow.iter().any(|allowed| {
                        path.starts_with(crate::inspect::job::normalize_path(allowed))
                    })
            })
            && !self.deny_prefixes.iter().any(|prefix| {
                let prefix = crate::inspect::job::normalize_path(prefix);
                path.as_os_str()
                    .to_string_lossy()
                    .starts_with(prefix.as_os_str().to_string_lossy().as_ref())
            })
            && !crate::project_content::is_excluded(&path)
    }
}

thread_local! { static CURRENT: RefCell<Option<Arc<Capture>>> = const { RefCell::new(None) }; }
#[cfg(test)]
thread_local! { static READ_AFTER_CHECK_FOR_TEST: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) }; }
#[cfg(test)]
type GitForTest = Box<dyn FnMut(&[&str], Instant) -> Option<Vec<u8>>>;
#[cfg(test)]
thread_local! {
    /// Stands in for the Git processes a diff starts, so a test can make one
    /// run out of time without depending on real process timing.
    static GIT_FOR_TEST: RefCell<Option<GitForTest>> = const { RefCell::new(None) };
}

/// One bounded Git read for a diff. `None` covers both a failed command and
/// one stopped at the deadline; callers check the deadline to tell them apart.
fn git_bytes(directory: &Path, args: &[&str], deadline: Instant, limit: usize) -> Option<Vec<u8>> {
    #[cfg(test)]
    if let Some(result) =
        GIT_FOR_TEST.with(|hook| hook.borrow_mut().as_mut().map(|run| run(args, deadline)))
    {
        return result;
    }
    crate::revision_read::bounded_git_bytes(directory, args, deadline, limit)
}
pub(crate) struct Scope(Option<Arc<Capture>>);
pub(crate) type SharedCallDeadline = Arc<Mutex<Option<Instant>>>;
pub(crate) struct BudgetGuard(SharedCallDeadline);
impl BudgetGuard {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self(Arc::new(Mutex::new(Some(deadline))))
    }
    pub(crate) fn shared(&self) -> SharedCallDeadline {
        self.0.clone()
    }
    pub(crate) fn clear(&self) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}
impl Drop for BudgetGuard {
    fn drop(&mut self) {
        self.clear();
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = std::mem::take(&mut self.0));
    }
}
pub(crate) fn scope(capture: Option<Arc<Capture>>) -> Scope {
    Scope(CURRENT.with(|current| current.replace(capture)))
}
pub(crate) fn current() -> Option<Arc<Capture>> {
    CURRENT.with(|current| current.borrow().clone())
}

impl Capture {
    #[cfg(test)]
    pub(crate) fn start(ctx: &crate::context::AppContext) -> Option<Arc<Self>> {
        Self::start_for_spawn(
            ctx,
            "bash_changed_files",
            &crate::sandbox_spawn::SpawnPlan::Unsandboxed,
        )
    }
    pub(crate) fn start_for_spawn(
        ctx: &crate::context::AppContext,
        request_id: &str,
        plan: &crate::sandbox_spawn::SpawnPlan,
    ) -> Option<Arc<Self>> {
        let mode = ctx.config().bash.changed_files;
        if mode == BashChangedFiles::Off {
            return None;
        }
        let root = ctx.config().project_root.clone()?;
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let journal = ctx
            .watcher_counters()
            .bash_changes
            .get_or_init(|| Arc::new(Journal::default()))
            .clone();
        let mut capture = Self::begin(root, journal, mode, ctx.watcher_runtime_active());
        if mode == BashChangedFiles::Diff {
            Arc::get_mut(&mut capture).unwrap().read_policy = ctx
                .access_policy(request_id)
                .ok()
                .map(|access| ReadPolicy::new(access, plan.changed_files_read_profile()));
        }
        Arc::get_mut(&mut capture).unwrap().flush = Some(ctx.watcher_counters().bash_flush.clone());
        if let Some(deadline) = crate::bash_background::changed_files_deadline_for_spawn() {
            Arc::get_mut(&mut capture).unwrap().call_deadline = deadline;
        }
        Some(capture)
    }
    fn begin(
        root: PathBuf,
        journal: Arc<Journal>,
        mode: BashChangedFiles,
        watcher_active: bool,
    ) -> Arc<Self> {
        // Git probes share one deadline and bounded output. Untracked paths are
        // classified from watcher create events, not an up-front directory scan.
        let git = if mode == BashChangedFiles::Diff {
            GitStart::read(&root)
        } else {
            None
        };
        let state = journal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (sequence, gaps, unreported_renames) =
            (state.sequence, state.gaps, state.unreported_renames);
        drop(state);
        Arc::new(Self {
            root,
            journal,
            sequence,
            gaps,
            unreported_renames,
            mode,
            watcher_active,
            flush: None,
            call_deadline: Arc::new(Mutex::new(None)),
            git,
            read_policy: None,
            end: OnceLock::new(),
            report: OnceLock::new(),
            #[cfg(test)]
            diff_delay_for_test: Duration::ZERO,
            #[cfg(test)]
            render_time_for_test: MAX_RENDER_TIME,
        })
    }
    pub(crate) fn finish(&self) -> &Report {
        self.report.get_or_init(|| {
            let started = Instant::now();
            #[cfg(not(test))]
            let render_time = MAX_RENDER_TIME;
            #[cfg(test)]
            let render_time = self.render_time_for_test;
            let deadline = self
                .call_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .map_or(started + render_time, |call| {
                    call.min(started + render_time)
                });
            let flush_deadline =
                deadline.min(started + crate::watcher_backend::flush::BASH_FLUSH_LIMIT);
            let caught_up = self.watcher_active
                && self
                    .flush
                    .as_ref()
                    .is_none_or(|flush| flush.wait(&self.root, flush_deadline));
            self.seal();
            self.render(caught_up, deadline)
        })
    }
    pub(crate) fn budget(self: &Arc<Self>, deadline: Option<Instant>) -> BudgetGuard {
        *self
            .call_deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = deadline;
        BudgetGuard(self.call_deadline.clone())
    }
    pub(crate) fn seal(&self) {
        self.end.get_or_init(|| {
            let state = self
                .journal
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (state.sequence, state.gaps, state.unreported_renames)
        });
    }
    fn render(&self, caught_up: bool, deadline: Instant) -> Report {
        let state = self
            .journal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (end_sequence, end_gaps, end_unreported_renames) =
            *self.end.get().expect("capture sealed before rendering");
        let unreported_rename = end_unreported_renames != self.unreported_renames;
        let overflow = end_gaps != self.gaps
            || state
                .events
                .front()
                .is_some_and(|(sequence, _, _, _)| *sequence > self.sequence + 1);
        let mut paths = BTreeMap::new();
        let comparison_root = comparison_path(&self.root);
        let mut list_deadline_cut = false;
        for (sequence, path, created, directory) in &state.events {
            if *sequence > self.sequence && *sequence <= end_sequence {
                if Instant::now() >= deadline {
                    list_deadline_cut = true;
                    break;
                }
                let path_key = comparison_path(path);
                if let Ok(relative) = path_key.strip_prefix(&comparison_root) {
                    if !relative.as_os_str().is_empty()
                        && !relative.components().any(|c| {
                            matches!(
                                c.as_os_str().to_str(),
                                Some(".git" | "target" | "node_modules")
                            )
                        })
                    {
                        if !directory {
                            paths.entry(relative.to_path_buf()).or_insert(*created);
                        }
                    }
                }
            }
        }
        drop(state);
        let total = paths.len();
        let mut text = HEADER.to_string();
        let mut cut = false;
        let mut pending = Vec::new();
        for (path, created) in paths {
            if pending.len() == MAX_FILES {
                cut = true;
                break;
            }
            if Instant::now() >= deadline {
                list_deadline_cut = true;
                break;
            }
            let absolute = self.root.join(&path);
            if absolute.is_dir() {
                continue;
            }
            let exists = absolute.symlink_metadata().is_ok();
            let file = ChangedFile {
                path: relative_path_text(&path),
                status: if !exists {
                    "deleted"
                } else if created {
                    "added"
                } else {
                    "modified"
                },
                diff: None,
                reason: None,
                diff_lines_list_envelope: None,
            };
            pending.push((path, created, exists, file));
        }
        // Collect the bounded path list before spending its remaining budget on
        // diffs, so a slow diff never hides later files we can already list.
        let diff_deadline = deadline
            - Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now()) / 10);
        let mut files = pending
            .iter()
            .map(|(_, _, _, file)| file.clone())
            .collect::<Vec<_>>();
        // Paths have first claim on the report budget. Only the path list itself,
        // never a diff, may force a later path out of the response.
        while report_body_bytes(&files) > MAX_REPORT_BYTES - 2048 {
            files.pop();
            pending.pop();
            cut = true;
        }
        for (index, (path, created, exists, _)) in pending.into_iter().enumerate() {
            let file = &mut files[index];
            if self.mode == BashChangedFiles::Diff {
                if let Some(git) = &self.git {
                    if Instant::now() < diff_deadline {
                        self.diff_file(git, &path, created, exists, diff_deadline, file);
                    }
                    // Every finished diff attempt leaves a diff or a reason;
                    // only the deadline stops one without either. Say so
                    // rather than leave the file looking diffable.
                    if file.diff.is_none() && file.reason.is_none() {
                        file.reason = Some(DIFF_BUDGET_SPENT.into());
                    }
                }
            }
            if report_body_bytes(&files) > MAX_REPORT_BYTES - 2048 {
                files[index].diff = None;
                files[index].diff_lines_list_envelope = None;
                // A new denial/skip reason may need space too. Reclaim earlier
                // diff space before considering the bounded path list itself.
                for earlier in (0..index).rev() {
                    if report_body_bytes(&files) <= MAX_REPORT_BYTES - 2048 {
                        break;
                    }
                    files[earlier].diff = None;
                    files[earlier].diff_lines_list_envelope = None;
                }
            }
        }
        while report_body_bytes(&files) > MAX_REPORT_BYTES - 2048 {
            files.pop();
            cut = true;
        }
        for file in &files {
            text.push_str(&render_file(file));
        }
        if files.is_empty() {
            text.push_str("\n  (no file changes observed)");
        }
        if self.mode == BashChangedFiles::Diff && self.git.is_none() {
            text.push('\n');
            text.push_str(
                if git_comparison_path(&self.root)
                    .ancestors()
                    .any(|path| path.join(".git").exists())
                {
                    GIT_UNAVAILABLE
                } else {
                    OUTSIDE_GIT
                },
            );
        }
        let mut incomplete = Vec::new();
        if overflow {
            incomplete.push(OVERFLOW.to_string());
        }
        if !caught_up || list_deadline_cut || unreported_rename {
            incomplete.push(NOT_CAUGHT_UP.to_string());
        }
        for reason in &incomplete {
            text.push('\n');
            text.push_str(reason);
        }
        let mut list_envelope =
            crate::list_surfaces::bash::build_changed_files_envelope_with_time_limit(
                files.len(),
                total,
                false,
                overflow || !caught_up || list_deadline_cut || unreported_rename,
                list_deadline_cut,
            );
        if cut && list_envelope.reason.is_none() {
            list_envelope.reason = Some(Reason::Cap);
            list_envelope.causes.push(Reason::Cap);
        }
        if list_envelope.reason.is_some() {
            let trailer = crate::list_surfaces::bash::envelope_trailer(&list_envelope);
            text.push('\n');
            text.push_str(&trailer);
        }
        Report {
            text,
            files,
            incomplete,
            changed_files_list_envelope: list_envelope,
        }
    }
    fn verified_file(
        &self,
        path: &Path,
    ) -> Result<Option<crate::commands::file_read_exact::OpenedFile>, ()> {
        use crate::commands::file_read_exact::{open_no_follow, opened_location, ReadExactRefusal};
        let policy = self.read_policy.as_ref().ok_or(())?;
        let root = self.root.canonicalize().map_err(|_| ())?;
        let target = root.join(path);
        if !policy.allows(&target) {
            return Err(());
        }
        #[cfg(test)]
        READ_AFTER_CHECK_FOR_TEST.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        match open_no_follow(&root, &target) {
            Ok(file) => {
                let metadata = file.metadata().map_err(|_| ())?;
                if crate::db::file_identity::is_live_sqlite_handle(&metadata)
                    || metadata.file_type().is_symlink()
                {
                    return Err(());
                }
                let opened = opened_location(&file).map_err(|_| ())?;
                // Compare the descriptor's location, not a second lookup of the
                // pathname. Reject Windows parent directories that redirect the
                // path through a junction or other reparse point, too.
                if comparison_path(&opened) != comparison_path(&target) || !policy.allows(&opened) {
                    return Err(());
                }
                Ok(Some(file))
            }
            Err(ReadExactRefusal::NotFound) => {
                // Git still records the deleted file's previous contents. Verify
                // its parent by handle before trusting the missing final component.
                let parent = target.parent().ok_or(())?;
                let directory = open_no_follow(&root, parent).map_err(|_| ())?;
                if !directory.metadata().map_err(|_| ())?.is_dir()
                    || comparison_path(&opened_location(&directory).map_err(|_| ())?)
                        != comparison_path(parent)
                {
                    return Err(());
                }
                Ok(None)
            }
            Err(_) => Err(()),
        }
    }

    fn diff_file(
        &self,
        git: &GitStart,
        path: &Path,
        _created: bool,
        _exists: bool,
        deadline: Instant,
        file: &mut ChangedFile,
    ) {
        if Instant::now() >= deadline {
            return;
        }
        #[cfg(test)]
        std::thread::sleep(
            self.diff_delay_for_test
                .min(deadline.saturating_duration_since(Instant::now())),
        );
        if Instant::now() >= deadline {
            return;
        }
        let absolute = self.root.join(path);
        let absolute_key = git_comparison_path(&absolute);
        if path.to_str().is_none() {
            file.reason = Some("no diff: path is not valid UTF-8".into());
            return;
        }
        let mut checked = match self.verified_file(path) {
            Ok(file) => file,
            Err(()) => {
                file.reason = Some(NOT_SHOWN.into());
                return;
            }
        };
        let exists = checked.is_some();
        for dirty in &git.dirty {
            if Instant::now() >= deadline {
                return;
            }
            if absolute_key.starts_with(git_comparison_path(dirty)) {
                file.status = if exists { "modified" } else { "deleted" };
                file.reason = Some(DIRTY.into());
                return;
            }
        }
        let git_root = git_comparison_path(&git.directory);
        let Ok(relative) = absolute_key.strip_prefix(&git_root) else {
            file.reason = Some("no diff: path is outside the git root".into());
            return;
        };
        let git_path = relative_path_text(relative);
        let spec = format!("{}:{}", git.head, git_path);
        let before = git_bytes(&git.directory, &["show", &spec], deadline, MAX_FILE_BYTES);
        if Instant::now() >= deadline {
            return;
        }
        let before = match before {
            Some(before) => {
                file.status = if exists { "modified" } else { "deleted" };
                before
            }
            None => {
                // `git show` also fails for a path HEAD does not have. An empty
                // tree listing tells that new file apart from an unreadable one.
                let entry = git_bytes(
                    &git.directory,
                    &["ls-tree", "-z", &git.head, "--", &git_path],
                    deadline,
                    4096,
                );
                if Instant::now() >= deadline {
                    return;
                }
                if !entry.is_some_and(|entry| entry.is_empty()) {
                    file.reason = Some(NO_BEFORE_IMAGE.into());
                    return;
                }
                file.status = if exists { "added" } else { "deleted" };
                Vec::new()
            }
        };
        if !before.is_empty() {
            file.status = if exists { "modified" } else { "deleted" };
        }
        let after = if let Some(opened) = checked.as_mut() {
            if Instant::now() >= deadline {
                return;
            }
            match opened.metadata() {
                Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_FILE_BYTES as u64 => {
                    use std::io::Read;
                    let mut bytes = Vec::new();
                    if (&mut **opened)
                        .take((MAX_FILE_BYTES + 1) as u64)
                        .read_to_end(&mut bytes)
                        .is_err()
                        || bytes.len() > MAX_FILE_BYTES
                    {
                        file.reason = Some("no diff: file is unreadable or too large".into());
                        return;
                    }
                    bytes
                }
                _ => {
                    file.reason =
                        Some("no diff: file is too large or is not a regular file".into());
                    return;
                }
            }
        } else {
            Vec::new()
        };
        if Instant::now() >= deadline {
            return;
        }
        let (Ok(before), Ok(after)) = (std::str::from_utf8(&before), std::str::from_utf8(&after))
        else {
            file.reason = Some("no diff: binary file".into());
            return;
        };
        if before.contains('\0') || after.contains('\0') {
            file.reason = Some("no diff: binary file".into());
            return;
        }
        let name = file.path.replace(['\n', '\r', '\t'], "?");
        let diff = similar::TextDiff::configure()
            .timeout(
                Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
            )
            .diff_lines(before, after);
        if Instant::now() >= deadline {
            return;
        }
        let rendered = diff
            .unified_diff()
            .header(
                &if file.status == "added" {
                    "/dev/null".into()
                } else {
                    format!("a/{name}")
                },
                &if file.status == "deleted" {
                    "/dev/null".into()
                } else {
                    format!("b/{name}")
                },
            )
            .to_string();
        if Instant::now() >= deadline {
            return;
        }
        let total = rendered.lines().count();
        let mut shown = 0;
        let mut capped = String::new();
        for line in rendered.lines() {
            if Instant::now() >= deadline {
                return;
            }
            if shown == MAX_DIFF_LINES || capped.len() + line.len() + 1 > MAX_REPORT_BYTES / 2 {
                break;
            }
            capped.push_str(line);
            capped.push('\n');
            shown += 1;
        }
        if shown < total {
            let envelope =
                crate::list_surfaces::bash::build_changed_files_envelope(shown, total, true, false);
            capped.push_str(&crate::list_surfaces::bash::envelope_trailer(&envelope));
            capped.push('\n');
            file.diff_lines_list_envelope = Some(envelope);
        }
        file.diff = Some(capped);
    }
}
impl GitStart {
    fn read(root: &Path) -> Option<Self> {
        let budget = GIT_CAPTURE_LIMIT;
        // Debug builds let correctness tests choose an ample or exhausted budget
        // without requiring Git process startup to meet a performance threshold.
        #[cfg(debug_assertions)]
        let budget = std::env::var("AFT_TEST_BASH_CHANGED_FILES_GIT_BUDGET_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value <= 30_000)
            .map(Duration::from_millis)
            .unwrap_or(budget);
        #[cfg(test)]
        let budget = TEST_GIT_CAPTURE_BUDGET
            .with(|slot| slot.get())
            .unwrap_or(budget);
        Self::read_with_git(root, budget, |directory, args, deadline| {
            crate::revision_read::bounded_git_bytes(directory, args, deadline, MAX_REPORT_BYTES)
        })
    }

    fn read_with_git(
        root: &Path,
        budget: Duration,
        mut git: impl FnMut(&Path, &[&str], Instant) -> Option<Vec<u8>>,
    ) -> Option<Self> {
        let root = comparison_path(root);
        let deadline = Instant::now() + budget;
        let mut run = |args: &[&str]| {
            if Instant::now() >= deadline {
                return None;
            }
            git(&root, args, deadline)
        };
        // One process returns both values; avoid paying for a second Git
        // startup before the dirty-file query, especially on loaded runners.
        let metadata =
            String::from_utf8(run(&["rev-parse", "--show-toplevel", "--verify", "HEAD"])?).ok()?;
        let (directory, head) = metadata.trim_end().rsplit_once('\n')?;
        let directory = PathBuf::from(directory.strip_suffix('\r').unwrap_or(directory));
        let head = head.trim().to_string();
        // Compare both index and worktree against the pinned HEAD, including
        // staged changes. Never refresh or write the caller's index.
        let bytes = run(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            "--no-renames",
        ])?;
        let dirty = bytes
            .split(|b| *b == 0)
            .filter(|p| p.len() >= 3)
            .map(|p| directory.join(String::from_utf8_lossy(&p[3..]).as_ref()))
            .collect();
        Some(Self {
            directory,
            head,
            dirty,
        })
    }
}

#[cfg(test)]
#[path = "bash_changed_files_test.rs"]
mod tests;
