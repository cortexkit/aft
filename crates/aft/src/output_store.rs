//! Session-owned saved tool results and their bounded storage cleanup.

use std::collections::HashMap;
use std::fs::ReadDir;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::context::AppContext;
use crate::file_identity::FileIdentity;

pub const SAVED_FILE_LIMIT: usize = 64 * 1024 * 1024;

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
pub(crate) struct Artifact {
    session: String,
    identity: FileIdentity,
}

#[derive(Default)]
pub(crate) struct OutputFiles {
    pub(crate) files: Mutex<HashMap<PathBuf, Artifact>>,
    roots: Mutex<HashMap<PathBuf, PathBuf>>,
    #[cfg(test)]
    pub(crate) canonicalizations: std::sync::atomic::AtomicUsize,
}

/// A deferred daemon (subc) tool reply can be sent after its AppContext is evicted.
/// Capture only the private output store and task registry needed to save it,
/// without retaining the project's indexes or other runtime state.
pub(crate) struct OutputScope {
    storage: PathBuf,
    directory: PathBuf,
    pub(crate) session: String,
    pub(crate) files: std::sync::Arc<OutputFiles>,
    pub(crate) bash: crate::bash_background::BgTaskRegistry,
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

    pub(crate) fn save(&self, scope: &OutputScope, text: &str) -> io::Result<PathBuf> {
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
