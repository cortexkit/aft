//! The final text budget is shared by every transport, not imposed by the host.

use std::collections::HashMap;
use std::fs::ReadDir;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::context::AppContext;
use crate::file_identity::FileIdentity;
use crate::protocol::Response;

pub const TEXT_LIMIT: usize = 50 * 1024;
/// Existing list producers reserve 10 KiB for their envelope, disclosures,
/// reminders and status bar rather than spending the final transport budget.
pub(crate) const LIST_BODY_BYTES: usize = TEXT_LIMIT - 10 * 1024;
pub const SAVED_FILE_LIMIT: usize = 64 * 1024 * 1024;
pub const CUT_TRAILER: &str = "[output cut at 50 KB of <total> KB; full result saved to <path>; read it with read and a line range]";
// Agents see this trailer, and the project owner approved its exact wording;
// keep it byte-identical, and get the owner's approval before changing it.
pub const SAVE_FAILED_TRAILER: &str =
    "[output cut at 50 KB of <total> KB; the full result could not be saved: <reason>]";

fn harness_root(ctx: &AppContext) -> PathBuf {
    let routed = crate::bash_background::task_storage_dir(ctx);
    if routed == ctx.storage_dir() {
        ctx.harness_opt()
            .map(|h| routed.join(h.storage_segment()))
            .unwrap_or(routed)
    } else {
        routed
    }
}

#[derive(Clone)]
struct Artifact {
    session: String,
    identity: FileIdentity,
}

#[derive(Default)]
pub(crate) struct OutputFiles {
    files: Mutex<HashMap<PathBuf, Artifact>>,
    roots: Mutex<HashMap<PathBuf, PathBuf>>,
    #[cfg(test)]
    canonicalizations: std::sync::atomic::AtomicUsize,
}

/// A deferred daemon (subc) tool reply can be sent after its AppContext is evicted.
/// Capture only the private output store and task registry needed to save it,
/// without retaining the project's indexes or other runtime state.
pub(crate) struct OutputScope {
    storage: PathBuf,
    directory: PathBuf,
    session: String,
    files: std::sync::Arc<OutputFiles>,
    bash: crate::bash_background::BgTaskRegistry,
}

impl std::fmt::Debug for OutputScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputScope")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

impl OutputScope {
    pub(crate) fn new(ctx: &AppContext, session: &str, harness: Option<&str>) -> Self {
        let storage = ctx.storage_dir();
        let harness_root = harness
            .and_then(|h| h.parse::<crate::harness::Harness>().ok())
            .map(|h| storage.join(h.storage_segment()))
            .unwrap_or_else(|| harness_root(ctx));
        Self {
            directory: harness_root
                .join("tool-results")
                .join(crate::backup::hash_session(session)),
            storage,
            session: session.into(),
            files: ctx.output_files_arc(),
            bash: ctx.bash_background().clone(),
        }
    }

    pub(crate) fn for_task(
        session_dir: &Path,
        session: &str,
        bash: crate::bash_background::BgTaskRegistry,
    ) -> Option<Self> {
        let harness = session_dir.parent()?.parent()?;
        Some(Self {
            storage: harness.to_path_buf(),
            directory: harness
                .join("tool-results")
                .join(crate::backup::hash_session(session)),
            session: session.into(),
            files: Default::default(),
            bash,
        })
    }
}

impl OutputFiles {
    pub(crate) fn shared() -> std::sync::Arc<Self> {
        let files = std::sync::Arc::new(Self::default());
        stores().lock().push(std::sync::Arc::downgrade(&files));
        files
    }

    fn session_dir(ctx: &AppContext, session: &str) -> PathBuf {
        harness_root(ctx)
            .join("tool-results")
            .join(crate::backup::hash_session(session))
    }

    fn save(&self, scope: &OutputScope, text: &str) -> io::Result<PathBuf> {
        if text.len() > SAVED_FILE_LIMIT {
            return Err(io::Error::other(
                "full tool result exceeds the 64 MiB saved-file limit",
            ));
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = &scope.storage;
        let dir = &scope.directory;
        crate::production_storage::refuse_write(&dir)?;
        crate::private_storage::open_dir(&root, &dir)?;
        let dir = self.canonicalize(dir)?;
        let mut files = self.files.lock();
        loop {
            let path = dir.join(format!(
                "{}-{}-{}.txt",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = match crate::private_storage::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            if let Err(error) = file.write_all(text.as_bytes()) {
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(error);
            }
            let identity = FileIdentity::of_file(&file);
            drop(file);
            if identity == FileIdentity::Unavailable {
                let _ = std::fs::remove_file(&path);
                return Err(io::Error::other("tool result file identity unavailable"));
            }
            let path = crate::path_identity::normalize_path(&path);
            files.insert(
                path.clone(),
                Artifact {
                    session: scope.session.clone(),
                    identity,
                },
            );
            if let (Some(raw), Some(canonical)) = (scope.directory.parent(), dir.parent()) {
                self.roots.lock().insert(
                    raw.to_path_buf(),
                    crate::path_identity::normalize_path(canonical),
                );
            }
            if let Some(harness) = scope.directory.parent().and_then(Path::parent) {
                scope.bash.retain_tool_output_store(harness);
            }
            return Ok(path);
        }
    }

    pub(crate) fn owner(&self, path: &Path) -> Option<String> {
        if !generated_name(path) {
            return None;
        }
        let path = crate::path_identity::normalize_path(path);
        self.files
            .lock()
            .get(&path)
            .map(|artifact| artifact.session.clone())
    }

    /// Re-register a saved result after the project's AppContext was evicted
    /// and rebuilt, which empties the in-memory table. Only generated names in
    /// the exact session folder qualify: a folder-prefix grant would expose
    /// other private stores or another session's results.
    pub(crate) fn restore(&self, ctx: &AppContext, session: &str, path: &Path) {
        if !generated_name(path) {
            return;
        }
        if self.owner(path).is_some() {
            return;
        }
        let Ok(path) = self.canonicalize(path) else {
            return;
        };
        let path = crate::path_identity::normalize_path(&path);
        if self.owner(&path).is_some() || !generated_name(&path) {
            return;
        }
        let dir = Self::session_dir(ctx, session);
        let Some(namespace) = self.canonical_root(ctx) else {
            return;
        };
        let expected = namespace.join(crate::backup::hash_session(session));
        if !std::fs::symlink_metadata(&dir)
            .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
            || path.parent() != Some(expected.as_path())
        {
            return;
        }
        let reopen = || -> io::Result<FileIdentity> {
            let (metadata, identity) = crate::file_identity::symlink_metadata(&path)?;
            check_metadata(&metadata)?;
            let file = open_verified(&path, identity)?;
            drop(file);
            Ok(identity)
        };
        if let Ok(identity) = reopen() {
            self.files.lock().entry(path).or_insert_with(|| Artifact {
                session: session.to_owned(),
                identity,
            });
        }
    }

    pub(crate) fn is_result_path(&self, ctx: &AppContext, path: &Path) -> bool {
        if !generated_name(path) {
            return false;
        }
        let Some(root) = self.canonical_root(ctx) else {
            return false;
        };
        path.strip_prefix(root)
            .is_ok_and(|relative| relative.components().count() == 2)
    }

    fn canonical_root(&self, ctx: &AppContext) -> Option<PathBuf> {
        let root = harness_root(ctx).join("tool-results");
        let mut roots = self.roots.lock();
        if let Some(canonical) = roots.get(&root) {
            return Some(canonical.clone());
        }
        let canonical = crate::path_identity::normalize_path(&self.canonicalize(&root).ok()?);
        roots.insert(root, canonical.clone());
        Some(canonical)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        #[cfg(test)]
        self.canonicalizations.fetch_add(1, Ordering::Relaxed);
        path.canonicalize()
    }

    pub(crate) fn read(&self, session: &str, path: &Path) -> Option<io::Result<Vec<u8>>> {
        if !generated_name(path) {
            return None;
        }
        let path = crate::path_identity::normalize_path(path);
        let artifact = self.files.lock().get(&path)?.clone();
        Some((|| {
            if artifact.session != session {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "tool result belongs to another session",
                ));
            }
            let mut file = open_verified(&path, artifact.identity)?;
            let mut bytes = Vec::new();
            (&mut *file)
                .take(SAVED_FILE_LIMIT as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > SAVED_FILE_LIMIT {
                return Err(io::Error::other("tool result exceeds saved-file limit"));
            }
            Ok(bytes)
        })())
    }
}

fn check_metadata(metadata: &std::fs::Metadata) -> io::Result<()> {
    if !metadata.is_file() || metadata.len() > SAVED_FILE_LIMIT as u64 {
        return Err(io::Error::other("not a bounded tool result file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(io::Error::other("tool result has multiple hard links"));
        }
    }
    Ok(())
}

fn open_verified(
    path: &Path,
    identity: FileIdentity,
) -> io::Result<crate::file_read_exact::OpenedFile> {
    if identity == FileIdentity::Unavailable {
        return Err(io::Error::other("tool result file identity unavailable"));
    }
    let root = path
        .parent()
        .ok_or_else(|| io::Error::other("tool result has no parent"))?;
    // Walk with O_NOFOLLOW on Unix; on Windows open reparse points without
    // following them. Verify the actual handle before reading any bytes.
    let file = crate::file_read_exact::open_no_follow(root, path)
        .map_err(|error| io::Error::other(format!("tool result open refused: {error:?}")))?;
    check_metadata(&file.metadata()?)?;
    if FileIdentity::of_file(&file) != identity
        || crate::path_identity::normalize_path(&crate::file_read_exact::opened_location(&file)?)
            != path
    {
        return Err(io::Error::other("tool result file was replaced"));
    }
    Ok(file)
}

fn stores() -> &'static Mutex<Vec<std::sync::Weak<OutputFiles>>> {
    static STORES: std::sync::OnceLock<Mutex<Vec<std::sync::Weak<OutputFiles>>>> =
        std::sync::OnceLock::new();
    STORES.get_or_init(Default::default)
}

/// Counts the directory entries and filesystem work examined in one cleanup pass.
#[derive(Debug, Default)]
pub(crate) struct CleanupReport {
    pub session_entries: usize,
    pub file_entries: usize,
    pub removed: usize,
    pub errors: usize,
    pub steps: usize,
    pub complete: bool,
}

struct CleanupCursor {
    sessions: ReadDir,
    files: Option<(PathBuf, ReadDir)>,
}

/// The task-artifact watchdog uses the same retention window as finished shell
/// output. The cursor resumes on the next bounded pass, including after eviction.
pub(crate) fn cleanup_expired(harness: &Path, retention: std::time::Duration) -> CleanupReport {
    const SCAN_ENTRIES: usize = 256;
    let root = crate::access_policy::policy_path(&harness.join("tool-results"));
    static CURSORS: std::sync::OnceLock<Mutex<HashMap<PathBuf, CleanupCursor>>> =
        std::sync::OnceLock::new();
    let mut cursors = CURSORS.get_or_init(Default::default).lock();
    if !cursors.contains_key(&root) {
        match std::fs::read_dir(&root) {
            Ok(sessions) => {
                cursors.insert(
                    root.clone(),
                    CleanupCursor {
                        sessions,
                        files: None,
                    },
                );
            }
            Err(error) => {
                let report = CleanupReport {
                    errors: usize::from(error.kind() != io::ErrorKind::NotFound),
                    complete: true,
                    ..Default::default()
                };
                log::debug!(
                    "tool result cleanup root={} examined={report:?}",
                    root.display()
                );
                return report;
            }
        }
    }
    let cursor = cursors.get_mut(&root).expect("output cleanup cursor");
    let expired = |path: &Path| {
        std::fs::symlink_metadata(path)
            .ok()
            .filter(|meta| meta.is_file() && !meta.file_type().is_symlink())
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age >= retention)
    };
    let mut report = CleanupReport::default();
    let mut removed = Vec::new();
    // Retain the two directory iterators across passes. A fresh iterator would
    // revisit unexpired entries forever and starve later sessions or files.
    for _ in 0..SCAN_ENTRIES {
        report.steps += 1;
        if let Some((_, files)) = &mut cursor.files {
            match files.next() {
                Some(Ok(file)) => {
                    report.file_entries += 1;
                    let path = file.path();
                    if generated_name(&path) && expired(&path) {
                        match std::fs::remove_file(&path) {
                            Ok(()) => {
                                report.removed += 1;
                                removed.push(path);
                            }
                            Err(_) => report.errors += 1,
                        }
                    }
                }
                Some(Err(_)) => {
                    report.file_entries += 1;
                    report.errors += 1;
                }
                None => {
                    let (session, entries) = cursor.files.take().unwrap();
                    drop(entries);
                    let _ = std::fs::remove_dir(session);
                }
            }
        } else {
            match cursor.sessions.next() {
                Some(Ok(session)) => {
                    report.session_entries += 1;
                    if session.file_type().is_ok_and(|kind| kind.is_dir()) {
                        match std::fs::read_dir(session.path()) {
                            Ok(files) => cursor.files = Some((session.path(), files)),
                            Err(_) => report.errors += 1,
                        }
                    }
                }
                Some(Err(_)) => {
                    report.session_entries += 1;
                    report.errors += 1;
                }
                None => {
                    report.complete = true;
                    break;
                }
            }
        }
    }
    if report.complete {
        cursors.remove(&root);
    }
    drop(cursors);
    if !removed.is_empty() {
        let mut stores = stores().lock();
        stores.retain(|store| {
            let Some(store) = store.upgrade() else {
                return false;
            };
            let mut files = store.files.lock();
            for path in &removed {
                files.remove(path);
            }
            true
        });
    }
    log::debug!(
        "tool result cleanup root={} examined={report:?}",
        root.display()
    );
    report
}

fn generated_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".txt"))
        .is_some_and(|name| {
            let mut pieces = name.split('-');
            for _ in 0..3 {
                let Some(piece) = pieces.next() else {
                    return false;
                };
                if piece.is_empty() || !piece.bytes().all(|byte| byte.is_ascii_digit()) {
                    return false;
                }
            }
            pieces.next().is_none()
        })
}

/// Called after reminders and the status bar, so the host never cuts off recovery text.
pub(crate) fn cap(
    ctx: &AppContext,
    session: &str,
    tool: &str,
    response: &Response,
    text: &mut String,
) {
    cap_with_scope(&OutputScope::new(ctx, session, None), tool, response, text);
}

pub(crate) fn cap_with_scope(
    scope: &OutputScope,
    tool: &str,
    response: &Response,
    text: &mut String,
) {
    if text.len() <= TEXT_LIMIT {
        if let Some((start, envelope)) = native_envelope_line(&response.data, text) {
            if start + envelope.len() < text.trim_end_matches('\n').len() {
                text.replace_range(start..start + envelope.len(), "");
                text.truncate(text.trim_end_matches('\n').len());
                text.push('\n');
                text.push_str(&envelope);
            }
        }
        return;
    }
    if cap_read(response, text).is_some() {
        return;
    }
    let total = text.len();
    let existing = response.data.get("output_path").and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .filter(|path| scope.bash.is_session_owned_artifact_path(&scope.session, path))
        // A task capture is reusable only if it contains the entire rendered reply,
        // not just stdout without stderr, formatting or appended reminders.
        .filter(|path| {
            std::fs::metadata(path).is_ok_and(|meta| meta.len() == total as u64)
                && scope.bash.read_artifact_path(&scope.session, path)
                    .is_some_and(|result| result.is_ok_and(|bytes| bytes == text.as_bytes()))
        });
    let path = existing
        .map(Ok)
        .unwrap_or_else(|| scope.files.save(scope, text));
    let trailer = match path {
        Ok(path) => CUT_TRAILER
            .replace("<total>", &format!("{:.1}", total as f64 / 1024.0))
            .replace("<path>", &path.display().to_string()),
        Err(error) => {
            log::error!(
                "could not save {tool} result for {}: {error}",
                scope.session
            );
            save_failed_trailer(total, &error.to_string())
        }
    };
    // A list's pagination contract remains visible even if its rows exceed
    // the transport budget. Save the original text, including these lines.
    let text_only = response.data.is_null()
        || response
            .data
            .as_object()
            .is_some_and(|object| object.is_empty());
    let list_metadata = std::iter::once(&response.data)
        .chain(response.data.get("details"))
        .filter_map(serde_json::Value::as_object)
        .any(|object| object.keys().any(|key| key.ends_with("_list_envelope")));
    let envelope = native_envelope_line(&response.data, text)
        .or_else(|| url_recovery_line(text))
        .map(|(_, line)| line)
        .unwrap_or_else(|| {
            if text_only || list_metadata {
                text.lines()
                    .rfind(|line| envelope_parts(line).is_some())
                    .unwrap_or("")
                    .to_owned()
            } else {
                String::new()
            }
        });
    let envelope_bytes = if envelope.is_empty() {
        0
    } else {
        envelope.len() + 1
    };
    if trailer.len() + envelope_bytes + 1 > TEXT_LIMIT {
        // Pathological batches can contain more trailers than fit in a reply.
        // They remain intact in the saved result rather than hiding recovery.
        render_cut_with_data(text, &trailer, "", &response.data);
    } else {
        render_cut_with_data(text, &trailer, &envelope, &response.data);
    }
}

/// Read already supplies a recovery footer. Reclaim whole source lines for its
/// marked tail and finalization notices instead of saving another copy of the file.
fn cap_read(response: &Response, text: &mut String) -> Option<()> {
    let data = std::iter::once(&response.data)
        .chain(response.data.get("details"))
        .find(|data| {
            data.get("read_output_tail")
                .is_some_and(serde_json::Value::is_u64)
                || data["read_output_frozen"] == true
        })?;
    let content = data.get("content")?.as_str()?;
    let start = text.find(content)?;
    let after_content = start + content.len();
    let paging = crate::subc_format::format_read_footer(false, data);
    let has_paging = !paging.is_empty() && text[after_content..].starts_with(&paging);
    let after_paging = after_content + if has_paging { paging.len() } else { 0 };
    if data["read_output_frozen"] == true {
        // Hashline publishes exactly the rendered rows. Keep that rendering
        // intact, spending its reserved headroom on the most recent notices.
        let allowance = TEXT_LIMIT.checked_sub(after_paging)?;
        let suffix = notice_suffix(&text[after_paging..], allowance).to_owned();
        text.replace_range(after_paging.., &suffix);
        return Some(());
    }
    let body_end = usize::try_from(data["read_output_tail"].as_u64()?).ok()?;
    let body = content.get(..body_end)?;
    let native_tail = content.get(body_end..)?;
    let mut appended = &text[after_paging..];
    let fixed_bytes = start + native_tail.len() + if has_paging { paging.len() } else { 0 };
    let remaining = TEXT_LIMIT.checked_sub(fixed_bytes)?;
    let first_line_bytes = body.find('\n').map_or(0, |newline| newline + 1);
    let notice_budget = remaining.saturating_sub(first_line_bytes);
    if appended.len() > notice_budget {
        // If notices alone exceed the budget, retain their whole-line suffix
        // (including the status bar) after the read recovery text.
        appended = notice_suffix(appended, notice_budget);
    }
    let allowance = remaining - appended.len();
    let end = body.as_bytes()[..allowance.min(body.len())]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1);
    let kept = body[..end].lines().count();
    let mut visible_data = data.clone();
    visible_data["lines_read"] = kept.into();
    let first = data.get("start_line")?.as_u64()?;
    visible_data["end_line"] = (first + kept.saturating_sub(1) as u64).into();
    let mut tail = native_tail.to_string();
    if data
        .get("scan_gap")
        .is_some_and(serde_json::Value::is_string)
    {
        tail = tail.replacen(
            &format!("\nshown {} of ", data["lines_read"]),
            &format!("\nshown {kept} of "),
            1,
        );
    }
    if has_paging {
        tail.push_str(&crate::subc_format::format_read_footer(
            false,
            &visible_data,
        ));
    }
    tail.push_str(appended);
    text.replace_range(start + end.., &tail);
    Some(())
}

fn notice_suffix(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let excess = text.len() - budget;
    let skip = text.as_bytes()[excess..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(text.len(), |newline| excess + newline);
    &text[skip..]
}

fn save_failed_trailer(total: usize, reason: &str) -> String {
    // An error may contain an oversized path. Reserve almost all of the budget
    // for useful tool text, and keep the recovery notice on one line.
    let reason = reason.replace(['\r', '\n'], " ");
    let end = reason.floor_char_boundary(reason.len().min(1024));
    SAVE_FAILED_TRAILER
        .replace("<total>", &format!("{:.1}", total as f64 / 1024.0))
        .replace("<reason>", &reason[..end])
}

#[cfg(test)]
fn render_cut(text: &mut String, trailer: &str, envelope: &str) {
    render_cut_with_data(text, trailer, envelope, &serde_json::Value::Null);
}

fn render_cut_with_data(
    text: &mut String,
    trailer: &str,
    envelope: &str,
    data: &serde_json::Value,
) {
    let envelope_bytes = if envelope.is_empty() {
        0
    } else {
        envelope.len() + 1
    };
    let mut end = TEXT_LIMIT.saturating_sub(trailer.len() + envelope_bytes + 1);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(newline) = text[..end].rfind('\n') {
        if newline >= end * 3 / 4 {
            end = newline;
        }
    }
    let corrected = envelope_parts(envelope)
        .map(|(shown, tail)| {
            let start = text.rfind(envelope).expect("rendered envelope");
            let visible = if start <= end {
                shown
            } else {
                visible_list_rows(data, text, &text[..end], tail).min(shown)
            };
            (start, format!("shown {visible} of {tail}"))
        })
        .or_else(|| {
            let (_, tail) = url_recovery_parts(envelope)?;
            let start = text.rfind(envelope)?;
            // The finalizer can cut again after appending a status bar. Keep the
            // URL's narrowing advice, but report only the source bytes still shown.
            Some((start, format!("truncated at {end} of {tail}")))
        });
    text.truncate(end);
    if let Some((start, _)) = &corrected {
        if *start < text.len() {
            let finish = (*start + envelope.len()).min(text.len());
            text.replace_range(*start..finish, "");
        }
    }
    text.truncate(text.trim_end_matches('\n').len());
    text.push('\n');
    text.push_str(trailer);
    if let Some((_, envelope)) = corrected {
        text.push('\n');
        text.push_str(&envelope);
    }
}

fn envelope_parts(line: &str) -> Option<(usize, &str)> {
    let (shown, tail) = line.strip_prefix("shown ")?.split_once(" of ")?;
    tail.contains(" (").then_some(())?;
    Some((shown.parse().ok()?, tail))
}

fn url_recovery_parts(line: &str) -> Option<(usize, &str)> {
    let (shown, tail) = line.strip_prefix("truncated at ")?.split_once(" of ")?;
    let (total, narrow) = tail.split_once(" bytes; narrow with ")?;
    let shown = shown.parse::<usize>().ok()?;
    (shown < total.parse::<usize>().ok()? && !narrow.is_empty()).then_some((shown, tail))
}

fn url_recovery_line(text: &str) -> Option<(usize, String)> {
    text.rmatch_indices("\ntruncated at ")
        .find_map(|(start, _)| {
            let start = start + 1;
            let line = text[start..].lines().next()?;
            url_recovery_parts(line)?;
            Some((start, line.to_owned()))
        })
}

fn native_envelope_line(data: &serde_json::Value, text: &str) -> Option<(usize, String)> {
    std::iter::once(data)
        .chain(data.get("details"))
        .filter_map(serde_json::Value::as_object)
        .flat_map(|object| object.iter())
        .filter(|(key, _)| key.ends_with("_list_envelope"))
        .filter_map(|(_, value)| {
            if value.get("reason").is_none_or(serde_json::Value::is_null) {
                return None;
            }
            let envelope =
                serde_json::from_value::<crate::list_envelope::ListEnvelope>(value.clone()).ok()?;
            let line = crate::subc_format::render_envelope_trailer(&envelope)?;
            let start = text
                .match_indices(line.as_str())
                .filter(|(start, _)| {
                    (*start == 0 || text.as_bytes()[*start - 1] == b'\n')
                        && text
                            .as_bytes()
                            .get(*start + line.len())
                            .is_none_or(|byte| *byte == b'\n')
                })
                .map(|(start, _)| start)
                .last()?;
            Some((start, line))
        })
        .max_by_key(|(start, _)| *start)
}

fn visible_list_rows(data: &serde_json::Value, full: &str, visible: &str, tail: &str) -> usize {
    let unit = tail
        .split(" (")
        .next()
        .unwrap_or("")
        .split_whitespace()
        .last()
        .unwrap_or("");
    if unit == "results"
        && data
            .get("matches")
            .and_then(serde_json::Value::as_array)
            .is_some()
    {
        // AST entries are rendered by this same helper. Walking complete row
        // strings avoids mistaking a captured source line for the next match.
        let mut cursor = 0;
        return data["matches"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| {
                let rendered = crate::subc_format::format_ast_search_match(row);
                let Some(offset) = full[cursor..].find(&rendered) else {
                    return false;
                };
                let start = cursor + offset;
                cursor = start + rendered.len();
                start + rendered.find('\n').unwrap_or(rendered.len()) <= visible.len()
            })
            .count();
    }
    if unit == "lines"
        && data
            .get("bash_output_list_envelope")
            .is_some_and(serde_json::Value::is_object)
    {
        let output = data
            .get("output_preview")
            .or_else(|| data.get("output"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim_end_matches('\n');
        if !output.is_empty() {
            if let Some(start) = full.find(output) {
                let end = visible.len().saturating_sub(start).min(output.len());
                return crate::list_surfaces::bash::count_output_lines(
                    &output[..output.floor_char_boundary(end)],
                );
            }
        }
        return 0;
    }
    let numbered_row = |line: &str| {
        line.trim_start()
            .split_once(": ")
            .is_some_and(|(number, _)| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            })
    };
    if matches!(unit, "rows" | "lines") && full.lines().any(numbered_row) {
        return visible.lines().filter(|line| numbered_row(line)).count();
    }
    if unit == "results" {
        return crate::subc_format::visible_search_result_count(data, full, visible);
    }
    if unit == "files" {
        return crate::subc_format::visible_file_row_count(data, full, visible);
    }
    let infer_from_text =
        data.is_null() || data.as_object().is_some_and(|object| object.is_empty());
    let tree_rows = data
        .get("tree_list_envelope")
        .is_some_and(serde_json::Value::is_object)
        || (infer_from_text
            && unit == "items"
            && full
                .lines()
                .filter_map(|line| line.strip_prefix("  ↳ "))
                .any(|line| line.contains(" [") || line.starts_with("+ ")));
    if tree_rows {
        return visible
            .lines()
            .filter_map(|line| line.strip_prefix("  ↳ "))
            .map(|line| {
                line.strip_prefix("+ ")
                    .and_then(|line| line.split_once(" unresolved external call"))
                    .and_then(|(count, _)| count.parse::<usize>().ok())
                    .unwrap_or(1)
            })
            .sum();
    }
    if unit == "items"
        && (data
            .get("callers_list_envelope")
            .is_some_and(serde_json::Value::is_object)
            || (infer_from_text && full.lines().any(|line| line.starts_with("  ↳ "))))
    {
        return visible
            .lines()
            .filter_map(|line| line.strip_prefix("  ↳ "))
            .map(|line| {
                line.split(':')
                    .find(|part| part.starts_with(|ch: char| ch.is_ascii_digit()))
                    .map(|part| {
                        part.chars()
                            .take_while(|ch| ch.is_ascii_digit() || matches!(ch, ',' | ' '))
                            .collect::<String>()
                    })
                    .map_or(0, |numbers| {
                        numbers
                            .split(',')
                            .filter(|number| number.trim().parse::<u64>().is_ok())
                            .count()
                    })
            })
            .sum();
    }
    if unit == "sites" {
        return visible
            .lines()
            .filter(|line| {
                !line.starts_with(' ')
                    && line
                        .rsplit_once(':')
                        .is_some_and(|(_, number)| number.parse::<u64>().is_ok())
            })
            .count();
    }
    if unit == "paths" {
        return visible
            .lines()
            .filter(|line| {
                line.strip_prefix("Path ")
                    .is_some_and(|number| number.parse::<usize>().is_ok())
            })
            .count();
    }
    if unit == "hops" {
        return visible
            .lines()
            .filter(|line| line.contains(" [") && line.contains(']'))
            .count();
    }
    if unit == "items"
        && (data
            .get("details")
            .is_some_and(serde_json::Value::is_object)
            || (infer_from_text
                && full.lines().any(|line| {
                    line.starts_with("Dead code:") || line.starts_with("diagnostics details")
                })))
    {
        return visible_inspect_rows(full, visible, tail);
    }
    // Text-only row lists carry one entry per non-empty line. Metadata and
    // their envelope are not entries in that list.
    visible
        .lines()
        .filter(|line| {
            !line.trim().is_empty()
                && envelope_parts(line).is_none()
                && !line.starts_with("[output ")
                && !line.starts_with("Found ")
        })
        .count()
}

fn visible_inspect_rows(full: &str, visible: &str, tail: &str) -> usize {
    let footer = format!(" of {tail}");
    let end = full.rfind(&footer).unwrap_or(full.len());
    let mut section = "";
    let mut start = 0;
    let mut offset = 0;
    let mut indent = 2;
    for line in full[..end].split_inclusive('\n') {
        let main = [
            "Dead code:",
            "Unused exports:",
            "Duplicates:",
            "Cyclomatic complexity:",
            "Import cycles:",
            "TODOs:",
            "diagnostics details",
            "Affected files:",
        ]
        .iter()
        .find(|heading| line.starts_with(**heading))
        .copied();
        if let Some(main) = main {
            section = main;
            start = offset + line.len();
            indent = 2;
        }
        if line.starts_with("  generated:") || line.starts_with("  test-only usage:") {
            start = offset + line.len();
            indent = 4;
        }
        offset += line.len();
    }
    if start >= visible.len() {
        return 0;
    }
    visible[start..]
        .lines()
        .filter(|line| {
            if section == "diagnostics details" {
                return line.starts_with("- ");
            }
            if line.starts_with("shown ") {
                return false;
            }
            let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
            if spaces != indent {
                return false;
            }
            match section {
                "Dead code:" | "Unused exports:" => line.contains("::"),
                "Duplicates:" => line.contains(" == "),
                "Cyclomatic complexity:" | "TODOs:" => line.contains(':'),
                "Import cycles:" => line.contains(" ["),
                "Affected files:" => !line.trim().is_empty(),
                _ => false,
            }
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn context(root: &Path, storage: &Path) -> AppContext {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.to_path_buf()),
                storage_dir: Some(storage.to_path_buf()),
                restrict_to_project_root: true,
                ..Default::default()
            },
        )
        .with_test_checkpoint_store();
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx
    }

    fn saved_path(text: &str) -> PathBuf {
        PathBuf::from(
            text.rsplit_once("; full result saved to ")
                .unwrap()
                .1
                .split_once("; read it with read and a line range]")
                .unwrap()
                .0,
        )
    }

    fn check(tool: &str, data: Value) {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let response = Response::success("cap", data.clone());
        let format =
            crate::subc_format::FormatContext::from_tool_call(tool, &json!({}), temp.path());
        let original = crate::subc_format::format_response_with_context(tool, &response, &format);
        eprintln!(
            "uncut backstop fixture: tool={tool} bytes={}",
            original.len()
        );
        assert!(
            original.len() > TEXT_LIMIT,
            "fixture did not exceed budget for {tool}"
        );
        let mut text = original.clone();
        cap(&ctx, "owner", tool, &response, &mut text);
        let path = saved_path(&text);
        assert!(path.is_absolute());
        // Compare resolved forms: on Windows the saved path and the storage
        // folder can differ in spelling (8.3 short names, the \\?\ prefix)
        // while naming the same folder.
        let results_dir = std::fs::canonicalize(ctx.harness_dir().join("tool-results")).unwrap();
        assert!(
            std::fs::canonicalize(&path)
                .unwrap()
                .starts_with(&results_dir),
            "{tool}: {} is not under {}",
            path.display(),
            results_dir.display()
        );
        assert!(text.len() <= 51_200, "{tool}: {}", text.len());
        let trailer = format!("[output cut at 50 KB of {:.1} KB; full result saved to {}; read it with read and a line range]", original.len() as f64 / 1024.0, path.display());
        assert!(text.ends_with(&trailer), "{tool}: incorrect trailer");
        assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
        assert_eq!(response.data, data, "structured content must remain uncut");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn status_bar_recapping_preserves_url_narrowing_and_visible_byte_count() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        ctx.update_status_bar_todos(Some(7));
        let source = "界".repeat(17_048);
        let original = format!(
            "{source}\ntruncated at {} of 90000 bytes; narrow with symbols",
            source.len()
        );
        assert!(original.len() <= TEXT_LIMIT);
        assert!(original.len() + "\n\n[AFT E? W? | T7]".len() > TEXT_LIMIT);
        let mut response = Response::success("url", json!({"text": original, "complete": false}));
        crate::response_finalize::finalize_response_with_bg_completions(
            &mut response,
            &ctx,
            "url-session",
            "outline",
            false,
        );
        let text = response.data["text"].as_str().unwrap();
        assert!(text.len() <= TEXT_LIMIT);
        let (visible, _) = text.split_once("\n[output cut").expect("final cap ran");
        assert!(
            text.ends_with(&format!(
                "truncated at {} of 90000 bytes; narrow with symbols",
                visible.len()
            )),
            "{text:?}"
        );
        assert_eq!(response.data["complete"], false);
        let saved = std::fs::read_to_string(saved_path(text)).unwrap();
        assert!(saved.ends_with("\n\n[AFT E? W? | T7]"));
        assert!(saved.contains("of 90000 bytes; narrow with symbols"));
    }

    #[test]
    fn tool_output_cap_zoom_batch() {
        let symbols: Vec<_> = (0..300)
            .map(|i| {
                json!({
                    "name": format!("symbol_{i}"),
                    "response": {"success":true, "name":format!("symbol_{i}"), "kind":"function",
                        "range":{"start_line":1,"end_line":10}, "content":"line();\n".repeat(100)}
                })
            })
            .collect();
        check("zoom", json!({"symbols":symbols}));
    }

    #[test]
    fn tool_output_cap_outline() {
        check(
            "outline",
            json!({"text":"  function outline_symbol() 1:20\n".repeat(6000)}),
        );
    }

    #[test]
    fn tool_output_cap_conflicts() {
        check(
            "conflicts",
            json!({"text":"1: <<<<<<< HEAD\n2: ours\n3: =======\n4: theirs\n5: >>>>>>> branch\n".repeat(3000)}),
        );
    }

    #[test]
    fn tool_output_cap_ast_search() {
        let matches: Vec<_> = (0..4000).map(|i| json!({"file":"src/main.ts", "line":i+1, "text":"console.log(long_variable_name);"})).collect();
        check(
            "ast_search",
            json!({"matches":matches, "total_matches":4000,"files_with_matches":1,"files_searched":1,"complete":true}),
        );
    }

    #[test]
    fn tool_output_cap_bash_status() {
        check(
            "bash_status",
            json!({"task_id":"bash-0123456789abcdef", "mode":"pipes", "status":"completed", "exit_code":0, "output_preview":"status output\n".repeat(12000)}),
        );
    }

    #[test]
    fn tool_output_cap_bash_and_watch_two_megabyte_line() {
        let output = "x".repeat(2 * 1024 * 1024);
        check(
            "bash",
            json!({"output":format!("Task bash-0123456789abcdef: completed\n{output}")}),
        );
        check(
            "bash_watch",
            json!({"output":format!("Task bash-0123456789abcdef: completed\nWaited 1ms; task exited.\n{output}"), "output_preview":output}),
        );
    }

    #[test]
    fn tool_output_cap_small_results_are_byte_identical() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        for original in ["", "already formatted\n", &"界".repeat(17066)] {
            let mut text = original.to_owned();
            cap(
                &ctx,
                "owner",
                "zoom",
                &Response::success("id", json!({})),
                &mut text,
            );
            assert_eq!(text, original);
        }
        assert!(ctx.output_files().files.lock().is_empty());
    }

    #[test]
    fn tool_output_cap_utf8_and_line_boundaries_and_list_envelope() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let response = Response::success("id", json!({}));
        for original in ["🦀界".repeat(20000), "a complete line 🦀\n".repeat(10000)] {
            let mut text = original.clone();
            cap(&ctx, "owner", "zoom", &response, &mut text);
            let prefix = text.split_once("\n[output cut").unwrap().0;
            assert!(original.starts_with(prefix));
            assert!(!prefix.contains('\u{fffd}'));
            if original.contains('\n') {
                assert!(prefix.ends_with("🦀"));
            } else {
                assert!(text.len() >= TEXT_LIMIT - 6);
            }
        }
        let envelope = "shown 20000 of 30000 rows (cap) · narrow: symbols";
        let mut text = format!("{}\n{envelope}", "row\n".repeat(20000));
        cap(&ctx, "owner", "zoom", &response, &mut text);
        let rows = text.lines().filter(|line| *line == "row").count();
        assert!(rows > 0 && rows < 20000);
        assert_eq!(
            text.lines().last().unwrap(),
            format!("shown {rows} of 30000 rows (cap) · narrow: symbols")
        );
        assert!(text
            .lines()
            .nth_back(1)
            .unwrap()
            .starts_with("[output cut at 50 KB of "));
        assert!(text.len() <= TEXT_LIMIT);
    }

    #[test]
    fn tool_output_cap_recounts_ast_and_grep_headers_and_keeps_envelope_last() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        for tool in ["ast_search", "grep"] {
            let matches: Vec<_> = (1..=2000).map(|line| json!({
                "file":"src/main.rs", "line":line, "text":"call(); ".repeat(30), "line_text":"call(); ".repeat(30),
            })).collect();
            let unit = if tool == "grep" {
                crate::list_envelope::Unit::Rows
            } else {
                crate::list_envelope::Unit::Results
            };
            let envelope = crate::list_envelope::ListEnvelope::new(
                2000,
                crate::list_envelope::Total::Exact(3000),
                unit,
                vec![crate::list_envelope::Reason::Budget],
                &["path"],
            );
            let mut data = json!({"matches":matches,"total_matches":3000,"files_searched":1,"files_with_matches":1,
                "matches_list_envelope":envelope});
            if tool == "grep" {
                data["text"] = json!(format!(
                    "src/main.rs\n{}",
                    (1..=2000)
                        .map(|line| format!("{line}: {}\n", "call(); ".repeat(30)))
                        .collect::<String>()
                ));
            }
            let response = Response::success("id", data.clone());
            let original = crate::subc_format::format_response(tool, &response, false);
            let mut text = original.clone();
            cap(&ctx, "owner", tool, &response, &mut text);
            let body = text.split_once("\n[output cut at ").unwrap().0;
            let shown = body
                .lines()
                .filter(|line| {
                    if tool == "grep" {
                        line.split_once(": ")
                            .is_some_and(|(number, _)| number.parse::<u64>().is_ok())
                    } else {
                        line.starts_with("src/main.rs:")
                    }
                })
                .count();
            assert!(shown > 0 && shown < 2000);
            assert_eq!(
                text.lines().last().unwrap(),
                format!("shown {shown} of 3000 {unit} (budget) · narrow: path")
            );
            assert_eq!(
                text.lines()
                    .filter(|line| line.starts_with("shown "))
                    .count(),
                1
            );
            assert_eq!(response.data, data, "structured content stays intact");
            assert_eq!(
                std::fs::read_to_string(saved_path(&text)).unwrap(),
                original
            );
            assert!(text.len() <= TEXT_LIMIT);
        }
    }

    #[test]
    fn tool_output_cap_keeps_native_list_envelope_after_finalization_notices_without_cutting() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let envelope = crate::list_envelope::ListEnvelope::new(
            2,
            crate::list_envelope::Total::Exact(3),
            crate::list_envelope::Unit::Rows,
            vec![crate::list_envelope::Reason::Cap],
            &["path"],
        );
        let line = crate::list_envelope::render_trailer(&envelope).unwrap();
        let response = Response::success("id", json!({"matches_list_envelope":envelope}));
        let mut text = format!("1: first\n2: second\n{line}\ncompletion reminder\nstatus bar");
        cap(&ctx, "owner", "grep", &response, &mut text);
        assert!(text.ends_with(&line));
        assert!(text.contains("completion reminder\nstatus bar"));
        assert_eq!(
            text.lines().filter(|row| row.starts_with("shown ")).count(),
            1
        );
        assert!(!text.contains("[output cut"));
        assert!(ctx.output_files().files.lock().is_empty());
    }

    #[test]
    fn tool_output_cap_read_keeps_native_footer_with_long_reminders_and_status() {
        for (ranged, artifact) in [(false, false), (true, false), (true, true)] {
            let temp = tempfile::tempdir().unwrap();
            let ctx = context(temp.path(), &temp.path().join("store"));
            let source: String = (1..=2000)
                .map(|line| format!("line-{line}-{}\n", "界".repeat(27)))
                .collect();
            let path = if artifact {
                let scope = OutputScope::new(&ctx, "owner", None);
                scope.files.save(&scope, &source).unwrap()
            } else {
                let path = temp.path().join("huge.txt");
                std::fs::write(&path, &source).unwrap();
                path
            };
            let files_before = ctx.output_files().files.lock().len();
            let mut params = json!({"file":path});
            if ranged {
                params["start_line"] = 31.into();
                params["end_line"] = 2000.into();
            }
            let request = crate::protocol::RawRequest {
                id: "read-cap".into(),
                command: "read".into(),
                lsp_hints: None,
                session_id: Some("owner".into()),
                params,
            };
            let response = crate::commands::read::handle_read(&request, &ctx);
            assert!(response.success, "{response:?}");
            let original_data = response.data.clone();
            let mut text = crate::subc_format::format_response("read", &response, ranged);
            let notices = format!(
                "\n<system-reminder>\n{}</system-reminder>\nstatus bar",
                "long completion reminder\n".repeat(800)
            );
            text.push_str(&notices);
            assert!(text.len() > TEXT_LIMIT);
            cap(&ctx, "owner", "read", &response, &mut text);

            assert!(text.len() <= TEXT_LIMIT, "{} bytes", text.len());
            assert!(text.ends_with(&notices));
            assert!(!text.contains("[output cut"));
            assert_eq!(ctx.output_files().files.lock().len(), files_before);
            assert_eq!(response.data, original_data);

            let rows: Vec<_> = text
                .lines()
                .take_while(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
                .collect();
            assert!(!rows.is_empty());
            let first = if ranged { 31 } else { 1 };
            for (index, row) in rows.iter().enumerate() {
                let (number, content) = row.trim_start().split_once(": ").unwrap();
                let number: usize = number.parse().unwrap();
                assert_eq!(number, first + index);
                assert_eq!(content, source.lines().nth(number - 1).unwrap());
            }
            let last = first + rows.len() - 1;
            assert!(last < response.data["end_line"].as_u64().unwrap() as usize);
            let last_row = *rows.last().unwrap();
            let tail = &text[text.find(last_row).unwrap() + last_row.len() + 1..];
            if artifact || !ranged {
                assert!(tail.starts_with(
                    "... (output truncated at 50KB, use start_line/end_line to read sections)\n"
                ));
            }
            if ranged && !artifact {
                assert!(tail.starts_with(&format!("\nshown {} of at least ", rows.len())));
                assert!(
                    tail.contains("(line or response scan budget) · narrow: start_line/end_line;")
                );
            }
            if !ranged {
                assert!(tail.contains(&format!(
                    "\n(Showing lines 1-{last} of 2000. Use startLine/endLine or offset/limit to read other sections.)"
                )));
            }

            let next = crate::protocol::RawRequest {
                params: json!({"file":path, "start_line":last + 1,"end_line":last + 1}),
                ..request
            };
            let next_response = crate::commands::read::handle_read(&next, &ctx);
            assert!(next_response.success, "{next_response:?}");
            assert_eq!(
                next_response.data["content"],
                format!("{}: {}\n", last + 1, source.lines().nth(last).unwrap())
            );
        }
    }

    #[test]
    fn tool_output_cap_read_keeps_recovery_when_notices_alone_exceed_limit() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let response = Response::success(
            "id",
            json!({
                "content":"1: first\n... (output truncated at 50KB, use start_line/end_line to read sections)\n",
                "read_output_tail":9,"lines_read":1,"start_line":1,"end_line":1,
                "total_lines":2000,"truncated":true
            }),
        );
        for notices in ["界".repeat(TEXT_LIMIT), "界\n".repeat(TEXT_LIMIT)] {
            let mut text = crate::subc_format::format_response("read", &response, false);
            text.push_str(&notices);
            text.push_str("\nstatus bar");
            cap(&ctx, "owner", "read", &response, &mut text);
            assert!(text.len() <= TEXT_LIMIT);
            assert!(text.starts_with(
                "1: first\n... (output truncated at 50KB, use start_line/end_line to read sections)\n"
            ));
            assert!(text.contains("Use startLine/endLine or offset/limit to read other sections."));
            assert!(text.ends_with("\nstatus bar"));
            assert!(!text.contains("[output cut"));
            assert!(ctx.output_files().files.lock().is_empty());
        }
    }

    #[test]
    fn tool_output_cap_hashline_read_preserves_footer_and_exact_seen_lines_with_long_reminders() {
        for ranged in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let ctx = context(&root, &root.join("store"));
            assert!(
                ctx.hashline_bindings()
                    .register(
                        &root,
                        "owner",
                        crate::hashline::integration::RegistrationRequest {
                            configured_enabled: true,
                            edit_slot_survives: true,
                            read_slot_survives: true,
                        }
                    )
                    .effective
            );
            let source: String = (1..=2000)
                .map(|line| {
                    format!(
                        "line-{line}-{}\n",
                        if line % 5 == 0 {
                            "y".into()
                        } else {
                            "界".repeat(27)
                        }
                    )
                })
                .collect();
            let path = root.join("huge-hashline.txt");
            std::fs::write(&path, &source).unwrap();
            let mut params = json!({"file":path, "_hashline_requested_path":"huge-hashline.txt"});
            if ranged {
                params["start_line"] = 1.into();
                params["end_line"] = 2000.into();
            }
            let response = crate::commands::read::handle_read(
                &crate::protocol::RawRequest {
                    id: "read-hashline-cap".into(),
                    command: "read".into(),
                    lsp_hints: None,
                    session_id: Some("owner".into()),
                    params,
                },
                &ctx,
            );
            assert!(response.success, "{response:?}");
            assert_eq!(response.data["read_output_frozen"], true);
            let content = response.data["content"].as_str().unwrap();
            let mut text = crate::subc_format::format_response("read", &response, ranged);
            text.push_str(&format!(
                "\n<system-reminder>\n{}</system-reminder>\nstatus bar",
                "long reminder\n".repeat(1600)
            ));
            assert!(text.len() > TEXT_LIMIT);
            cap(&ctx, "owner", "read", &response, &mut text);
            assert!(text.len() <= TEXT_LIMIT);
            assert!(
                text.starts_with(content),
                "published rendering must remain byte-identical"
            );
            assert!(text.ends_with("\nstatus bar"));
            assert!(!text.contains("[output cut"));
            assert!(ctx.output_files().files.lock().is_empty());

            let shown: std::collections::BTreeSet<usize> = text
                .lines()
                .skip(1)
                .take_while(|line| line.starts_with(|c: char| c.is_ascii_digit()))
                .map(|line| {
                    let (number, body) = line.split_once(':').unwrap();
                    let number: usize = number.parse().unwrap();
                    assert_eq!(body, source.lines().nth(number - 1).unwrap());
                    number
                })
                .collect();
            assert!(!shown.is_empty());
            let omitted: Vec<_> = (1..=2000).filter(|line| !shown.contains(line)).collect();
            assert!(!omitted.is_empty());
            let footer = format!("... (output truncated at 50KB, use start_line/end_line to read sections; lines {}-{} are not addressable)\n", omitted.first().unwrap(), omitted.last().unwrap());
            assert!(content.ends_with(&footer));
            assert!(text.contains(&footer));
            let guard = ctx
                .hashline_bindings()
                .capture(root.clone(), "owner")
                .unwrap();
            guard.with_binding(|binding| {
                let (_, snapshot) = binding.snapshots().iter().next().unwrap();
                assert_eq!(snapshot.coverage.seen_lines, shown);
                assert_eq!(
                    snapshot
                        .records
                        .keys()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>(),
                    shown
                );
                for line in 1..=2000 {
                    assert_eq!(snapshot.is_seen(line), shown.contains(&line), "line {line}");
                }
            });
        }
    }

    #[test]
    fn tool_output_cap_does_not_invent_envelopes_for_unbudgeted_text() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let original = format!(
            "shown 1 of 2 files (cap) · narrow: path\n{}",
            "symbol\n".repeat(20_000)
        );
        let response = Response::success("id", json!({"text":original}));
        let mut text = original.clone();
        cap(&ctx, "owner", "outline", &response, &mut text);
        assert!(text.starts_with("shown 1 of 2 files (cap) · narrow: path\n"));
        assert!(text.ends_with("; read it with read and a line range]"));
    }

    #[test]
    fn tool_output_cap_recounts_legacy_flat_glob_file_rows() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let files: Vec<_> = (0..20)
            .map(|index| format!("{}file_{index}.rs", "deep/".repeat(600)))
            .collect();
        let envelope = crate::list_envelope::ListEnvelope::new(
            20,
            crate::list_envelope::Total::Exact(30),
            crate::list_envelope::Unit::Files,
            vec![crate::list_envelope::Reason::Cap],
            &["path"],
        );
        let response = Response::success(
            "id",
            json!({"files":files,"text":format!("20 files matching *.rs\n\n{}",files.join("\n")),"files_list_envelope":envelope}),
        );
        let mut text = crate::subc_format::format_response("glob", &response, false);
        cap(&ctx, "owner", "glob", &response, &mut text);
        let shown = text
            .split_once("\n[output cut")
            .unwrap()
            .0
            .lines()
            .filter(|line| line.starts_with("deep/") && line.ends_with(".rs"))
            .count();
        assert!(shown > 0 && shown < 20);
        assert_eq!(
            text.lines().last().unwrap(),
            format!("shown {shown} of 30 files (cap) · narrow: path")
        );
        assert!(text.len() <= TEXT_LIMIT);
    }

    #[test]
    fn tool_output_cap_recounts_search_results_with_repeated_symbol_names() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let results: Vec<_> = (0..1000).map(|i| json!({"file":format!("src/file_{i}.rs"), "name":"shared", "kind":"function", "start_line":1, "end_line":2})).collect();
        let text = (0..1000)
            .map(|i| {
                format!(
                    "src/file_{i}.rs\n  shared [function] lines 1-2\n      {}\n\n",
                    "body ".repeat(50)
                )
            })
            .collect::<String>();
        let envelope = crate::list_envelope::ListEnvelope::new(
            1000,
            crate::list_envelope::Total::Exact(2000),
            crate::list_envelope::Unit::Results,
            vec![crate::list_envelope::Reason::Cap],
            &["topK"],
        );
        let response = Response::success(
            "id",
            json!({"results":results,"text":text,"results_list_envelope":envelope}),
        );
        let mut text = crate::subc_format::format_response("search", &response, false);
        cap(&ctx, "owner", "search", &response, &mut text);
        let shown = text
            .split_once("\n[output cut")
            .unwrap()
            .0
            .lines()
            .filter(|line| line.starts_with("  shared ["))
            .count();
        assert!(shown > 0 && shown < 1000);
        assert_eq!(
            text.lines().last().unwrap(),
            format!("shown {shown} of 2000 results (cap) · narrow: topK")
        );
        assert!(text.len() <= TEXT_LIMIT);
    }

    #[test]
    fn tool_output_cap_weights_coalesced_callers_and_unresolved_tree_children() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        for (key, row, weight) in [
            ("callers_list_envelope", "  ↳ run:9, 12\n", 2),
            ("callers_list_envelope", "  ↳ operator []:9, 12\n", 2),
            (
                "tree_list_envelope",
                "  ↳ + 5 unresolved external calls: a, b\n",
                5,
            ),
        ] {
            let total = 10_000 * weight;
            let envelope = format!("shown {total} of {} items (cap) · narrow: depth", total + 1);
            let mut text = format!("root\n{}\n{envelope}", row.repeat(10_000));
            let response = Response::success("id", json!({key:{}}));
            cap(&ctx, "owner", "callgraph", &response, &mut text);
            let count = text
                .split_once("\n[output cut")
                .unwrap()
                .0
                .lines()
                .filter(|line| line.starts_with("  ↳ "))
                .count()
                * weight;
            assert!(count > 0 && count < total);
            assert_eq!(
                text.lines().last().unwrap(),
                format!("shown {count} of {} items (cap) · narrow: depth", total + 1)
            );
        }
    }

    #[test]
    fn tool_output_cap_does_not_count_an_unreached_inspect_section() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let mut text = format!("Dead code:\n{}\nshown 10000 of 20000 items (cap) · narrow: scope\n  generated: 10:\n{}\nshown 10 of 20 items (cap) · narrow: scope",
            "  src/main.rs::unused\n".repeat(10_000), "    generated.rs::unused\n".repeat(10));
        let envelope = crate::list_envelope::ListEnvelope::new(
            10,
            crate::list_envelope::Total::Exact(20),
            crate::list_envelope::Unit::Items,
            vec![crate::list_envelope::Reason::Cap],
            &["scope"],
        );
        let response = Response::success(
            "id",
            json!({"details":{"dead_code_generated_list_envelope":envelope}}),
        );
        cap(&ctx, "owner", "inspect", &response, &mut text);
        assert!(text.ends_with("shown 0 of 20 items (cap) · narrow: scope"));
        assert!(text.len() <= TEXT_LIMIT);
    }

    #[test]
    fn tool_output_cap_owner_reads_range_while_confined_foreign_session_refused() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let ctx = context(&root, &temp.path().join("store"));
        let original = format!("first\nsecond\n{}", "later\n".repeat(20000));
        let mut text = original.clone();
        cap(
            &ctx,
            "owner",
            "zoom",
            &Response::success("id", json!({})),
            &mut text,
        );
        let path = saved_path(&text);
        let request = |session: &str| crate::protocol::RawRequest {
            id: "read-cap".into(),
            command: "read".into(),
            lsp_hints: None,
            session_id: Some(session.into()),
            params: json!({"file":path, "start_line":2,"end_line":2}),
        };
        let response = crate::context::confinement::with_root(&ctx, "read-cap", || {
            Ok(crate::commands::read::handle_read(&request("owner"), &ctx))
        })
        .unwrap();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["content"], "2: second\n");
        let foreign = crate::commands::read::handle_read(&request("foreign"), &ctx);
        assert!(!foreign.success, "{foreign:?}");
        // A second context for the same session can recover a persisted result.
        let restored = context(&root, &temp.path().join("store"));
        let restored_read = crate::commands::read::handle_read(&request("owner"), &restored);
        assert!(restored_read.success, "{restored_read:?}");
        assert_eq!(restored_read.data["content"], "2: second\n");
        assert!(!crate::commands::read::handle_read(&request("foreign"), &restored).success);
        ctx.with_force_restrict("read-cap", || {
            assert!(ctx
                .validate_read_path("read-cap", "foreign", &path)
                .is_err());
        });
        drop(restored);
        cleanup_expired(&ctx.harness_dir(), std::time::Duration::ZERO);
        assert!(!path.exists());
    }

    #[test]
    fn tool_output_cap_saved_file_limit_refuses_instead_of_claiming_full_result() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let mut text = "x".repeat(SAVED_FILE_LIMIT + 1);
        cap(
            &ctx,
            "owner",
            "zoom",
            &Response::success("id", json!({})),
            &mut text,
        );
        assert!(text.len() <= TEXT_LIMIT);
        assert_eq!(
            SAVE_FAILED_TRAILER,
            "[output cut at 50 KB of <total> KB; the full result could not be saved: <reason>]"
        );
        assert!(
            text.starts_with(&"x".repeat(38_000)),
            "save failure must retain useful output"
        );
        assert!(text.ends_with("[output cut at 50 KB of 65536.0 KB; the full result could not be saved: full tool result exceeds the 64 MiB saved-file limit]"));
        assert!(!text.contains("full result saved"));
        assert!(ctx.output_files().files.lock().is_empty());
    }

    #[test]
    fn tool_output_cap_golden() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let mut text = format!("header\n{}", "x".repeat(65536 - 7));
        cap(
            &ctx,
            "owner",
            "conflicts",
            &Response::success("id", json!({})),
            &mut text,
        );
        let path = saved_path(&text);
        let (prefix, trailer) = text.rsplit_once("\n[output cut").unwrap();
        let long_line = prefix.strip_prefix("header\n").unwrap();
        assert!(long_line.len() >= 38_000);
        assert!(long_line.bytes().all(|byte| byte == b'x'));
        let normalized = format!("header\n<long-line prefix>\n[output cut{trailer}")
            .replace(&path.display().to_string(), "<path>");
        assert_eq!(
            normalized,
            include_str!("../tests/fixtures/tool-result-cap.txt").trim_end()
        );
    }

    #[test]
    fn tool_output_cap_header_and_two_megabyte_line_keep_useful_utf8_prefix() {
        let original = format!("header\n{}", "界".repeat(700_000));
        let mut text = original.clone();
        render_cut(&mut text, "[trailer]", "");
        let prefix = text.strip_suffix("\n[trailer]").unwrap();
        assert!(prefix.len() >= 38_400 && prefix.len() <= 51_200);
        assert!(original.starts_with(prefix));
        assert!(!prefix.contains('\u{fffd}'));
    }

    #[test]
    fn tool_output_cap_read_access_fast_paths_do_not_canonicalize_ordinary_paths() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let files = ctx.output_files();
        let ordinary = temp.path().join("ordinary.txt");
        files.restore(&ctx, "owner", &ordinary);
        assert!(!files.is_result_path(&ctx, &ordinary));
        assert!(files.owner(&ordinary).is_none());
        assert!(files.read("owner", &ordinary).is_none());
        assert_eq!(files.canonicalizations.load(Ordering::Relaxed), 0);
        let mut text = "row\n".repeat(20_000);
        cap(
            &ctx,
            "owner",
            "zoom",
            &Response::success("id", json!({})),
            &mut text,
        );
        let path = saved_path(&text);
        let before = files.canonicalizations.load(Ordering::Relaxed);
        for _ in 0..50 {
            files.restore(&ctx, "owner", &path);
            assert!(files.is_result_path(&ctx, &path));
            assert_eq!(files.owner(&path).as_deref(), Some("owner"));
        }
        assert_eq!(files.canonicalizations.load(Ordering::Relaxed), before);
        let restored = context(temp.path(), &temp.path().join("store"));
        assert!(restored.output_files().is_result_path(&restored, &path));
        let before = restored
            .output_files()
            .canonicalizations
            .load(Ordering::Relaxed);
        assert!(before > 0, "a new context must resolve its root once");
        for _ in 0..50 {
            assert!(restored.output_files().is_result_path(&restored, &path));
        }
        assert_eq!(
            restored
                .output_files()
                .canonicalizations
                .load(Ordering::Relaxed),
            before
        );
    }

    #[test]
    fn tool_output_cap_read_rejects_replacement_with_identical_size_and_times() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let original = "x".repeat(60_000);
        let mut text = original.clone();
        cap(
            &ctx,
            "owner",
            "zoom",
            &Response::success("id", json!({})),
            &mut text,
        );
        let path = saved_path(&text);
        assert_eq!(
            ctx.output_files().read("owner", &path).unwrap().unwrap(),
            original.as_bytes()
        );
        crate::file_identity::replace_keeping_times(&path, &vec![b'y'; original.len()]);
        ctx.output_files().restore(&ctx, "owner", &path);
        assert!(ctx.output_files().read("owner", &path).unwrap().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn tool_output_cap_read_refuses_symlink_and_hardlink_replacements() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let mut text = "x".repeat(60_000);
        cap(
            &ctx,
            "owner",
            "zoom",
            &Response::success("id", json!({})),
            &mut text,
        );
        let path = saved_path(&text);
        let renamed = path.with_extension("original");
        std::fs::rename(&path, &renamed).unwrap();
        std::os::unix::fs::symlink(&renamed, &path).unwrap();
        assert!(ctx.output_files().read("owner", &path).unwrap().is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&renamed, &path).unwrap();
        assert!(ctx.output_files().read("owner", &path).unwrap().is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tool_output_cap_saved_results_do_not_hold_file_descriptors() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let mut paths = Vec::new();
        for _ in 0..20 {
            let mut text = "row\n".repeat(20_000);
            cap(
                &ctx,
                "owner",
                "zoom",
                &Response::success("id", json!({})),
                &mut text,
            );
            paths.push(saved_path(&text));
        }
        let descriptors: Vec<_> = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|target| paths.contains(target))
            .collect();
        assert!(
            descriptors.is_empty(),
            "saved files still open: {descriptors:?}"
        );
        for path in paths {
            assert!(ctx.output_files().read("owner", &path).unwrap().is_ok());
        }
    }

    #[test]
    fn tool_output_cap_save_failure_bounds_oversized_reason_without_losing_body() {
        let trailer = save_failed_trailer(70_000, &format!("{}\nreason", "界".repeat(30_000)));
        let mut text = "body\n".repeat(20_000);
        render_cut(&mut text, &trailer, "");
        assert!(text.len() <= 51_200);
        assert!(text.starts_with(&"body\n".repeat(7500)));
        assert_eq!(text.lines().last().unwrap(), trailer);
    }

    #[test]
    fn tool_output_cap_retention_cleans_live_and_restarted_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let mut text = "row\n".repeat(20_000);
        cap(
            &ctx,
            "owner",
            "zoom",
            &Response::success("id", json!({})),
            &mut text,
        );
        let path = saved_path(&text);
        let mut restarted = "orphaned row\n".repeat(20_000);
        {
            let other = context(temp.path(), &temp.path().join("store"));
            cap(
                &other,
                "restarted",
                "zoom",
                &Response::success("id", json!({})),
                &mut restarted,
            );
        }
        let persisted = saved_path(&restarted);
        cleanup_expired(&ctx.harness_dir(), std::time::Duration::from_secs(3600));
        assert!(path.exists() && persisted.exists());
        cleanup_expired(&ctx.harness_dir(), std::time::Duration::ZERO);
        assert!(!path.exists() && !persisted.exists());
        assert!(ctx.output_files().owner(&path).is_none());
    }

    fn make_expired(path: &Path) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
    }

    fn finish_cleanup(harness: &Path) -> (usize, usize, usize) {
        let mut sessions = 0;
        let mut files = 0;
        let mut removed = 0;
        for _ in 0..20 {
            let report = cleanup_expired(harness, std::time::Duration::from_secs(3600));
            assert!(report.steps <= 256, "{report:?}");
            assert!(
                report.session_entries + report.file_entries <= report.steps,
                "{report:?}"
            );
            assert_eq!(report.errors, 0, "{report:?}");
            sessions += report.session_entries;
            files += report.file_entries;
            removed += report.removed;
            if report.complete {
                return (sessions, files, removed);
            }
        }
        panic!("bounded cleanup cursor never completed");
    }

    #[test]
    fn tool_output_cap_retention_progresses_past_256_unexpired_session_folders() {
        let temp = tempfile::tempdir().unwrap();
        let mut old = Vec::new();
        let mut fresh = Vec::new();
        for i in 0..300 {
            let dir = temp
                .path()
                .join("tool-results")
                .join(format!("session-{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            let expired = dir.join("1-1-1.txt");
            let current = dir.join("1-1-2.txt");
            std::fs::write(&expired, "old").unwrap();
            std::fs::write(&current, "fresh").unwrap();
            make_expired(&expired);
            old.push(expired);
            fresh.push(current);
        }
        assert_eq!(finish_cleanup(temp.path()), (300, 600, 300));
        assert!(old.iter().all(|path| !path.exists()));
        assert!(fresh.iter().all(|path| path.exists()));
    }

    #[test]
    fn tool_output_cap_retention_progresses_past_256_unexpired_files() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("tool-results").join("session");
        std::fs::create_dir_all(&dir).unwrap();
        let mut old = Vec::new();
        let mut fresh = Vec::new();
        for i in 0..350 {
            let expired = dir.join(format!("1-1-{}.txt", i * 2));
            let current = dir.join(format!("1-1-{}.txt", i * 2 + 1));
            std::fs::write(&expired, "old").unwrap();
            std::fs::write(&current, "fresh").unwrap();
            make_expired(&expired);
            old.push(expired);
            fresh.push(current);
        }
        assert_eq!(finish_cleanup(temp.path()), (1, 700, 350));
        assert!(old.iter().all(|path| !path.exists()));
        assert!(fresh.iter().all(|path| path.exists()));
    }

    #[test]
    fn tool_output_cap_redisplayed_bash_defers_cut_until_final_rendering() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let body = "shell output\n".repeat(20_000);
        let mut response = Response::success("id", json!({"output":body}));
        let mut text = format!("{body}\ncompletion reminder\nstatus bar");
        crate::response_finalize::finalize_redisplayed_bash_response(
            &mut response,
            &mut text,
            &ctx,
            "owner",
            false,
        );
        assert!(text.starts_with(&body));
        assert!(
            text.len() > TEXT_LIMIT,
            "a redisplay still needs the whole body and suffix"
        );
        let original = text.clone();
        cap(&ctx, "owner", "bash", &response, &mut text);
        assert!(text.len() <= TEXT_LIMIT);
        assert_eq!(
            std::fs::read_to_string(saved_path(&text)).unwrap(),
            original
        );
    }
}
