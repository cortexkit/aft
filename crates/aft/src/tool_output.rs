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
pub const SAVED_FILE_LIMIT: usize = 64 * 1024 * 1024;
pub const CUT_TRAILER: &str = "[output cut at 50 KB of <total> KB; full result saved to <path>; read it with read and a line range]";
// Approved by Ufuk as agent-visible wording; keep this trailer byte-identical.
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

/// A deferred subc tool reply can be sent after its AppContext is evicted.
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
            let path = crate::inspect::job::normalize_path(&path);
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
                    crate::inspect::job::normalize_path(canonical),
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
        let path = crate::inspect::job::normalize_path(path);
        self.files
            .lock()
            .get(&path)
            .map(|artifact| artifact.session.clone())
    }

    /// Reopen only generated names in the exact session namespace after an
    /// actor has been evicted. A directory-prefix grant would expose other
    /// private stores or another session's results.
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
        let path = crate::inspect::job::normalize_path(&path);
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
        let canonical = crate::inspect::job::normalize_path(&self.canonicalize(&root).ok()?);
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
        let path = crate::inspect::job::normalize_path(path);
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
) -> io::Result<crate::commands::file_read_exact::OpenedFile> {
    if identity == FileIdentity::Unavailable {
        return Err(io::Error::other("tool result file identity unavailable"));
    }
    let root = path
        .parent()
        .ok_or_else(|| io::Error::other("tool result has no parent"))?;
    // Walk with O_NOFOLLOW on Unix; on Windows open reparse points without
    // following them. Verify the actual handle before reading any bytes.
    let file = crate::commands::file_read_exact::open_no_follow(root, path)
        .map_err(|error| io::Error::other(format!("tool result open refused: {error:?}")))?;
    check_metadata(&file.metadata()?)?;
    if FileIdentity::of_file(&file) != identity
        || crate::inspect::job::normalize_path(&crate::commands::file_read_exact::opened_location(
            &file,
        )?) != path
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

/// Counts the directory entries and filesystem work examined in one GC pass.
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
    let envelopes: Vec<_> = text
        .lines()
        .filter(|line| line.starts_with("shown ") && line.contains(" · narrow:"))
        .collect();
    let envelope = envelopes.join("\n");
    let envelope_bytes = if envelope.is_empty() {
        0
    } else {
        envelope.len() + 1
    };
    if trailer.len() + envelope_bytes + 1 > TEXT_LIMIT {
        // Pathological batches can contain more trailers than fit in a reply.
        // They remain intact in the saved result rather than hiding recovery.
        render_cut(text, &trailer, "");
    } else {
        render_cut(text, &trailer, &envelope);
    }
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

fn render_cut(text: &mut String, trailer: &str, envelope: &str) {
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
    text.truncate(end);
    if !envelope.is_empty() {
        text.push('\n');
        text.push_str(envelope);
    }
    text.push('\n');
    text.push_str(trailer);
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
                .strip_suffix("; read it with read and a line range]")
                .unwrap(),
        )
    }

    fn check(tool: &str, data: Value) {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let response = Response::success("cap", data.clone());
        let format =
            crate::subc_format::FormatContext::from_tool_call(tool, &json!({}), temp.path());
        let original = crate::subc_format::format_response_with_context(tool, &response, &format);
        assert!(
            original.len() > TEXT_LIMIT,
            "fixture did not exceed budget for {tool}"
        );
        let mut text = original.clone();
        cap(&ctx, "owner", tool, &response, &mut text);
        let path = saved_path(&text);
        assert!(path.is_absolute());
        assert!(path.starts_with(ctx.harness_dir().join("tool-results")));
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
        let envelope = "shown 100 of 200 symbols (cap) · narrow: symbols";
        let mut text = format!("{}\n{envelope}", "row\n".repeat(20000));
        cap(&ctx, "owner", "zoom", &response, &mut text);
        assert!(text.contains(&format!("\n{envelope}\n[output cut")));
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
