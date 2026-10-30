//! Reading a file as it is at a git revision: the `ref` argument of `read`,
//! `outline` and `zoom`.
//!
//! Content comes only from the repository's object store. Git is asked to
//! resolve the revision to a commit, to look the path up in that commit's
//! tree, and to print the blob; nothing here runs a command that reads or
//! writes the working tree, the index or HEAD. The blob is copied into a
//! private scratch directory under AFT's storage directory, keeping the
//! file's repository-relative path so the existing read, outline and zoom
//! code (and language detection by file name) work on it unchanged. The
//! scratch directory is removed when the [`RevisionSession`] is dropped.
//!
//! The feature is off unless the `revision_reads` config key is true. While
//! it is off, a call that passes `ref` is refused with the typed code
//! [`DISABLED_CODE`]; the tool schemas do not advertise the argument either.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Native request parameter carrying the revision.
pub(crate) const REF_PARAM: &str = "ref";

/// Error code for a `ref` call while `revision_reads` is off.
pub(crate) const DISABLED_CODE: &str = "revision_reads_disabled";

/// Time allowed for all git commands one call runs together.
const GIT_DEADLINE: Duration = Duration::from_secs(10);

/// Largest blob copied out of the object store. Above this the call is
/// refused before any content is read. Blobs up to this size then follow the
/// normal `read` rules (a whole read above `read`'s own limit is refused with
/// the advice to read a line range).
pub(crate) const MAX_REVISION_BLOB_BYTES: u64 = 256 * 1024 * 1024;

/// Longest revision string accepted.
const MAX_REF_LEN: usize = 256;

/// Output kept from git's small answers (rev-parse, ls-tree) and stderr.
const SMALL_OUTPUT_BYTES: usize = 64 * 1024;

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

/// The `ref` argument of a request, when it carries a non-blank one.
pub(crate) fn requested_ref(req: &RawRequest) -> Option<&str> {
    req.params
        .get(REF_PARAM)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// Refuse the call unless `revision_reads` is on.
pub(crate) fn check_enabled(req: &RawRequest, ctx: &AppContext) -> Result<(), Response> {
    if ctx.config().revision_reads {
        return Ok(());
    }
    Err(Response::error(
        &req.id,
        DISABLED_CODE,
        format!(
            "{}: 'ref' was refused because reading at a git revision is turned off. Set \"revision_reads\": true in the AFT config to enable it, or call without 'ref' to read the working tree.",
            req.command
        ),
    ))
}

/// One call's view of a git revision: the revision resolved to a commit once,
/// and the files of that commit copied out so far.
pub(crate) struct RevisionSession {
    reference: String,
    commit: String,
    toplevel: PathBuf,
    deadline: Instant,
    scratch_parent: PathBuf,
    scratch: RefCell<Option<ScratchDir>>,
    materialized: RefCell<HashMap<String, PathBuf>>,
    symbol_cache: crate::parser::SharedSymbolCache,
}

impl Drop for RevisionSession {
    fn drop(&mut self) {
        // Staged paths are unique to this request and can never yield a cache
        // hit later. Remove their symbols before the scratch files disappear.
        let mut cache = self
            .symbol_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for path in self.materialized.get_mut().values() {
            cache.invalidate(path);
        }
    }
}

impl RevisionSession {
    /// Resolve `reference` to a commit of the session's repository.
    ///
    /// The repository is the one holding the configured project root (a
    /// linked worktree resolves to its own checkout, which shares refs and
    /// objects with the main one). Without a project root, `anchor` (the
    /// requested path) locates it.
    pub(crate) fn open(
        req: &RawRequest,
        ctx: &AppContext,
        reference: &str,
        anchor: Option<&Path>,
    ) -> Result<Self, Response> {
        check_enabled(req, ctx)?;
        let reference = validate_reference(req, reference)?;
        let deadline = Instant::now() + GIT_DEADLINE;
        let start = ctx
            .config()
            .project_root
            .clone()
            .or_else(|| anchor.map(existing_directory))
            .ok_or_else(|| {
                Response::error(
                    &req.id,
                    "revision_unavailable",
                    format!(
                        "{}: 'ref' needs a git repository, and no project root or path names one",
                        req.command
                    ),
                )
            })?;
        let toplevel_output = run_git(
            &start,
            [OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
            deadline,
            Sink::memory(SMALL_OUTPUT_BYTES),
        )
        .map_err(|failure| failure.response(req, "find the git repository"))?;
        if !toplevel_output.status.success() {
            return Err(Response::error(
                &req.id,
                "revision_unavailable",
                format!(
                    "{}: 'ref' needs a git repository, and {} is not inside one",
                    req.command,
                    start.display()
                ),
            ));
        }
        let toplevel = output_line(&toplevel_output.stdout)
            .map(PathBuf::from)
            .ok_or_else(|| {
                Response::error(
                    &req.id,
                    "revision_unavailable",
                    format!("{}: git did not name the repository root", req.command),
                )
            })?;
        let toplevel = std::fs::canonicalize(&toplevel).unwrap_or(toplevel);

        let mut spec = reference.clone();
        spec.push_str("^{commit}");
        let commit_output = run_git(
            &toplevel,
            [
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("--quiet"),
                OsStr::new("--end-of-options"),
                OsStr::new(&spec),
            ],
            deadline,
            Sink::memory(SMALL_OUTPUT_BYTES),
        )
        .map_err(|failure| failure.response(req, "resolve the revision"))?;
        let commit = output_line(&commit_output.stdout)
            .filter(|_| commit_output.status.success())
            .filter(|line| is_object_id(line))
            .ok_or_else(|| {
                Response::error(
                    &req.id,
                    "revision_not_found",
                    format!(
                        "{}: ref '{reference}' does not name a commit in this repository",
                        req.command
                    ),
                )
            })?
            .to_string();

        Ok(Self {
            reference,
            commit,
            toplevel,
            deadline,
            scratch_parent: ctx.storage_dir().join("revision-reads"),
            scratch: RefCell::new(None),
            materialized: RefCell::new(HashMap::new()),
            symbol_cache: ctx
                .provider()
                .as_any()
                .downcast_ref::<crate::parser::TreeSitterProvider>()
                .map(|provider| provider.symbol_cache())
                .unwrap_or_else(|| ctx.symbol_cache()),
        })
    }

    /// The `revision` member added to every successful reply.
    pub(crate) fn reply_field(&self) -> Value {
        json!({ "ref": self.reference, "commit": self.commit })
    }

    /// Add [`Self::reply_field`] to a successful response.
    pub(crate) fn annotate(&self, response: &mut Response) {
        if response.success {
            if let Some(data) = response.data.as_object_mut() {
                data.insert("revision".into(), self.reply_field());
            }
        }
    }

    /// Map a materialized revision file back to its checkout path for host read checks.
    pub(crate) fn logical_path(&self, staged: &Path) -> Option<PathBuf> {
        self.materialized
            .borrow()
            .iter()
            .find_map(|(relative, path)| (path == staged).then(|| self.toplevel.join(relative)))
    }

    /// Copy `requested` (the path argument, already checked against the read
    /// rules by the caller) out of the commit, returning the copy's path.
    /// `display` is how the reply names the path.
    pub(crate) fn materialize(
        &self,
        req: &RawRequest,
        ctx: &AppContext,
        requested: &Path,
        display: &str,
    ) -> Result<PathBuf, Response> {
        let relative = self.repository_path(req, ctx, requested, display)?;
        if let Some(existing) = self.materialized.borrow().get(&relative) {
            return Ok(existing.clone());
        }

        let listing = run_git(
            &self.toplevel,
            [
                OsStr::new("ls-tree"),
                OsStr::new("-l"),
                OsStr::new("-z"),
                OsStr::new("--full-tree"),
                OsStr::new(&self.commit),
                OsStr::new("--"),
                OsStr::new(&relative),
            ],
            self.deadline,
            Sink::memory(SMALL_OUTPUT_BYTES),
        )
        .map_err(|failure| failure.response(req, "look the path up at the revision"))?;
        if !listing.status.success() {
            return Err(Response::error(
                &req.id,
                "revision_unavailable",
                format!(
                    "{}: git could not list '{display}' at ref '{}' (commit {}): {}",
                    req.command,
                    self.reference,
                    self.commit,
                    String::from_utf8_lossy(&listing.stderr).trim()
                ),
            ));
        }
        let entry = parse_tree_entry(&listing.stdout, &relative).ok_or_else(|| {
            Response::error(
                &req.id,
                "revision_path_not_found",
                format!(
                    "{}: '{display}' does not exist at ref '{}' (commit {})",
                    req.command, self.reference, self.commit
                ),
            )
        })?;
        match entry.kind.as_str() {
            "blob" => {}
            "tree" => {
                return Err(Response::error(
                    &req.id,
                    "revision_path_is_directory",
                    format!(
                        "{}: '{display}' is a directory at ref '{}' (commit {}); 'ref' reads single files",
                        req.command, self.reference, self.commit
                    ),
                ))
            }
            other => {
                return Err(Response::error(
                    &req.id,
                    "unsupported_file_type",
                    format!(
                        "{}: '{display}' is a {other} entry (a submodule) at ref '{}' (commit {}); 'ref' reads files only",
                        req.command, self.reference, self.commit
                    ),
                ))
            }
        }
        if entry.size > MAX_REVISION_BLOB_BYTES {
            return Err(Response::error(
                &req.id,
                "revision_blob_too_large",
                format!(
                    "{}: '{display}' is {} bytes at ref '{}' (commit {}), above the {} byte limit for reading at a revision",
                    req.command, entry.size, self.reference, self.commit, MAX_REVISION_BLOB_BYTES
                ),
            ));
        }

        let target = self.scratch_path(req, &relative)?;
        let file = std::fs::File::create(&target).map_err(|error| {
            Response::error(
                &req.id,
                "io_error",
                format!(
                    "{}: cannot stage the blob for reading: {error}",
                    req.command
                ),
            )
        })?;
        let blob = run_git(
            &self.toplevel,
            [
                OsStr::new("cat-file"),
                OsStr::new("blob"),
                OsStr::new(&entry.object),
            ],
            self.deadline,
            Sink::file(file, MAX_REVISION_BLOB_BYTES),
        )
        .map_err(|failure| failure.response(req, "read the blob"))?;
        if !blob.status.success() {
            return Err(Response::error(
                &req.id,
                "revision_unavailable",
                format!(
                    "{}: git could not read '{display}' at ref '{}' (commit {}): {}",
                    req.command,
                    self.reference,
                    self.commit,
                    String::from_utf8_lossy(&blob.stderr).trim()
                ),
            ));
        }
        self.materialized
            .borrow_mut()
            .insert(relative, target.clone());
        Ok(target)
    }

    /// The path's location inside the repository, `/`-separated as git
    /// spells tree paths. Only the spelling of the checkout root is resolved;
    /// every component below it is taken as written. Today's symlinks say
    /// nothing about the revision: a directory that is a link now may have
    /// been a real directory then, and a final symlink is read as the entry
    /// the revision stores under that name.
    fn repository_path(
        &self,
        req: &RawRequest,
        ctx: &AppContext,
        requested: &Path,
        display: &str,
    ) -> Result<String, Response> {
        let absolute = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            ctx.config()
                .project_root
                .clone()
                .unwrap_or_else(|| self.toplevel.clone())
                .join(requested)
        };
        let normalized = crate::path_identity::lexical_normalize(&absolute);
        let outside = || {
            Response::error(
                &req.id,
                "revision_path_outside_repository",
                format!(
                    "{}: '{display}' is outside the repository at {}; 'ref' reads files of the session's own repository",
                    req.command,
                    self.toplevel.display()
                ),
            )
        };
        let relative = repository_relative(&normalized, &self.toplevel).ok_or_else(outside)?;
        let mut parts = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_str().ok_or_else(|| {
                    Response::error(
                        &req.id,
                        "invalid_request",
                        format!("{}: '{display}' is not valid UTF-8", req.command),
                    )
                })?),
                _ => return Err(outside()),
            }
        }
        if parts.is_empty() {
            return Err(Response::error(
                &req.id,
                "revision_path_is_directory",
                format!(
                    "{}: '{display}' is the repository root; 'ref' reads single files",
                    req.command
                ),
            ));
        }
        // The caller already checked the path against the project root, but
        // that check follows today's symlinks. The git tree lookup that uses
        // these components does not: a path through a link that points into
        // the root today names whatever the commit stored under the link's
        // own name, which can lie outside the root. So the verbatim location
        // is checked against the root too.
        let location = parts
            .iter()
            .fold(self.toplevel.clone(), |path, part| path.join(part));
        ctx.check_location_lexically_in_root(&req.id, requested, &location)?;
        ctx.check_historical_read_location(&req.id, &location)?;
        Ok(parts.join("/"))
    }

    fn scratch_path(&self, req: &RawRequest, relative: &str) -> Result<PathBuf, Response> {
        let io_error = |error: std::io::Error| {
            Response::error(
                &req.id,
                "io_error",
                format!(
                    "{}: cannot stage the blob for reading: {error}",
                    req.command
                ),
            )
        };
        let mut scratch = self.scratch.borrow_mut();
        if scratch.is_none() {
            *scratch = Some(ScratchDir::create(&self.scratch_parent).map_err(io_error)?);
        }
        let root = &scratch.as_ref().expect("scratch was just created").0;
        let target = relative
            .split('/')
            .fold(root.clone(), |path, part| path.join(part));
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        Ok(target)
    }
}

/// A private directory removed, with everything in it, when dropped.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn create(parent: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(parent)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let sequence = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("{}-{nanos}-{sequence}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Refuse revision strings git could read as an option, or that are not text.
fn validate_reference(req: &RawRequest, reference: &str) -> Result<String, Response> {
    let reference = reference.trim();
    let invalid = |why: &str| {
        Response::error(
            &req.id,
            "invalid_request",
            format!("{}: 'ref' {why}", req.command),
        )
    };
    if reference.starts_with('-') {
        return Err(invalid("must not start with '-'"));
    }
    if reference.len() > MAX_REF_LEN {
        return Err(invalid(&format!("is longer than {MAX_REF_LEN} characters")));
    }
    if reference.chars().any(char::is_control) {
        return Err(invalid("must not contain control characters"));
    }
    Ok(reference.to_string())
}

/// The nearest directory at or above `path` that exists.
fn existing_directory(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|ancestor| ancestor.is_dir())
        .unwrap_or(path)
        .to_path_buf()
}

/// `path` relative to the checkout whose resolved root is `toplevel`, with
/// the components below the root kept exactly as written. The root may be
/// spelled differently from git's resolved form (a symlinked temp directory,
/// say), so the shortest ancestor of `path` that resolves to `toplevel` is
/// taken as the root. Searching from the shortest ancestor means a symlink
/// inside the checkout is never resolved, even one pointing back at the root.
/// `None` when no ancestor is the checkout root.
fn repository_relative(path: &Path, toplevel: &Path) -> Option<PathBuf> {
    if let Ok(relative) = path.strip_prefix(toplevel) {
        return Some(relative.to_path_buf());
    }
    let mut ancestors = path.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    ancestors
        .into_iter()
        .find(|ancestor| std::fs::canonicalize(ancestor).is_ok_and(|resolved| resolved == toplevel))
        .and_then(|root| path.strip_prefix(root).ok())
        .map(Path::to_path_buf)
}

fn output_line(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then_some(line)
}

fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

struct TreeEntry {
    kind: String,
    object: String,
    size: u64,
}

/// Find `relative` in `git ls-tree -l -z` output, whose records read
/// `<mode> SP <type> SP <object> SP+ <size> TAB <path> NUL`.
fn parse_tree_entry(output: &[u8], relative: &str) -> Option<TreeEntry> {
    output.split(|byte| *byte == 0).find_map(|record| {
        let record = std::str::from_utf8(record).ok()?;
        let (meta, path) = record.split_once('\t')?;
        if path != relative {
            return None;
        }
        let mut fields = meta.split_whitespace();
        let _mode = fields.next()?;
        let kind = fields.next()?.to_string();
        let object = fields.next()?.to_string();
        let size = fields
            .next()
            .and_then(|size| size.parse().ok())
            .unwrap_or(0);
        is_object_id(&object).then_some(TreeEntry { kind, object, size })
    })
}

/// Where a git command's standard output goes, and how much of it is allowed.
enum Sink {
    Memory { limit: u64 },
    File { file: std::fs::File, limit: u64 },
}

impl Sink {
    fn memory(limit: usize) -> Self {
        Self::Memory {
            limit: limit as u64,
        }
    }

    fn file(file: std::fs::File, limit: u64) -> Self {
        Self::File { file, limit }
    }
}

struct GitOutput {
    status: ExitStatus,
    /// Captured standard output; empty when it was written to a file.
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

enum GitFailure {
    Spawn(std::io::Error),
    Timeout,
    TooLarge,
    Io(std::io::Error),
}

impl GitFailure {
    fn response(self, req: &RawRequest, step: &str) -> Response {
        let (code, detail) = match self {
            Self::Spawn(error) => (
                "revision_unavailable",
                format!("git could not start: {error}"),
            ),
            Self::Timeout => (
                "revision_timeout",
                format!(
                    "git did not finish within {} seconds",
                    GIT_DEADLINE.as_secs()
                ),
            ),
            Self::TooLarge => (
                "revision_blob_too_large",
                format!("the output exceeded {MAX_REVISION_BLOB_BYTES} bytes"),
            ),
            Self::Io(error) => ("io_error", error.to_string()),
        };
        Response::error(
            &req.id,
            code,
            format!("{}: could not {step}: {detail}", req.command),
        )
    }
}

/// Run one read-only git command in `directory` under a deadline, killing it
/// (and anything it started) when the deadline passes or its output grows
/// past the sink's limit.
pub(crate) fn bounded_git_bytes(
    directory: &Path,
    args: &[&str],
    deadline: Instant,
    limit: usize,
) -> Option<Vec<u8>> {
    let output = run_git(directory, args, deadline, Sink::memory(limit)).ok()?;
    output.status.success().then_some(output.stdout)
}

fn run_git<I, S>(
    directory: &Path,
    args: I,
    deadline: Instant,
    sink: Sink,
) -> Result<GitOutput, GitFailure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = crate::git_probe::command();
    command
        .arg("-C")
        .arg(directory)
        .args(args)
        // Paths are file names, never patterns or pathspec magic.
        .env("GIT_LITERAL_PATHSPECS", "1")
        // Never take the index lock for an opportunistic refresh.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::format::isolate_in_process_group(&mut command);
    let mut child = command.spawn().map_err(GitFailure::Spawn)?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let overflow = Arc::new(AtomicBool::new(false));
    let (closed_tx, closed_rx) = mpsc::channel::<()>();

    let stdout_closed = closed_tx.clone();
    let stdout_overflow = overflow.clone();
    let stdout_thread = std::thread::spawn(move || {
        let result = copy_limited(stdout, sink, &stdout_overflow);
        let _ = stdout_closed.send(());
        result
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut captured = Vec::new();
        let _ = stderr
            .take(SMALL_OUTPUT_BYTES as u64)
            .read_to_end(&mut captured);
        let _ = closed_tx.send(());
        captured
    });

    let mut open_pipes = 2u8;
    let status = loop {
        if overflow.load(Ordering::Acquire) {
            stop(&mut child);
            return Err(GitFailure::TooLarge);
        }
        let now = Instant::now();
        if now >= deadline {
            stop(&mut child);
            return Err(GitFailure::Timeout);
        }
        let wait = (deadline - now).min(Duration::from_millis(20));
        if open_pipes > 0 {
            match closed_rx.recv_timeout(wait) {
                Ok(()) => open_pipes -= 1,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => open_pipes = 0,
            }
            continue;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(wait.min(Duration::from_millis(2))),
            Err(error) => {
                stop(&mut child);
                return Err(GitFailure::Io(error));
            }
        }
    };
    let stdout = stdout_thread
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("output reader panicked")))
        .map_err(GitFailure::Io)?;
    if overflow.load(Ordering::Acquire) {
        return Err(GitFailure::TooLarge);
    }
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok(GitOutput {
        status,
        stdout,
        stderr,
    })
}

fn stop(child: &mut Child) {
    crate::format::kill_process_tree(child);
    let _ = child.wait();
}

/// Copy `reader` into the sink, stopping (and raising `overflow`) once more
/// than the sink's limit has arrived. Returns the captured bytes for a
/// memory sink.
fn copy_limited(
    mut reader: impl Read,
    sink: Sink,
    overflow: &AtomicBool,
) -> std::io::Result<Vec<u8>> {
    let (mut file, limit) = match sink {
        Sink::Memory { limit } => (None, limit),
        Sink::File { file, limit } => (Some(file), limit),
    };
    let mut captured = Vec::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        total += read as u64;
        if total > limit {
            overflow.store(true, Ordering::Release);
            return Ok(captured);
        }
        match file.as_mut() {
            Some(file) => file.write_all(&buffer[..read])?,
            None => captured.extend_from_slice(&buffer[..read]),
        }
    }
    if let Some(file) = file.as_mut() {
        file.flush()?;
    }
    Ok(captured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_entry_parser_matches_only_the_exact_path() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let output =
            format!("100644 blob {oid}     12\tsrc/a.rs\0040000 tree {oid}       -\tsrc\0");
        let entry = parse_tree_entry(output.as_bytes(), "src/a.rs").expect("blob entry");
        assert_eq!(entry.kind, "blob");
        assert_eq!(entry.size, 12);
        assert_eq!(
            parse_tree_entry(output.as_bytes(), "src").unwrap().kind,
            "tree"
        );
        assert!(parse_tree_entry(output.as_bytes(), "src/b.rs").is_none());
    }

    #[test]
    fn option_like_and_control_character_refs_are_refused() {
        let req: RawRequest =
            serde_json::from_value(json!({"id": "r", "command": "read"})).unwrap();
        assert!(validate_reference(&req, "--output=/tmp/x").is_err());
        assert!(validate_reference(&req, "main\nHEAD").is_err());
        assert_eq!(validate_reference(&req, " HEAD~1 ").unwrap(), "HEAD~1");
    }
}
