//! Read-only Git index snapshots and counted, deadline-bound Git metadata queries.
//!
//! A snapshot records the index bytes observed, not proof that working-tree bytes
//! are clean. Callers must verify working-tree content independently.

use crate::file_identity::FileIdentity;
use sha1::Digest;
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    /// The format whose object IDs are `hex_len` hex digits long.
    pub fn from_hex_len(hex_len: usize) -> Option<Self> {
        match hex_len {
            40 => Some(Self::Sha1),
            64 => Some(Self::Sha256),
            _ => None,
        }
    }

    /// Git's name for the format (`extensions.objectFormat`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }

    /// The object ID Git gives `bytes` stored as a blob: the digest of
    /// `blob <length>\0<bytes>`. These are the exact bytes read, so the caller
    /// must already know that no conversion applies between them and the blob.
    pub fn blob_oid(self, bytes: &[u8]) -> Vec<u8> {
        self.object_oid(b"blob", bytes)
    }

    fn object_oid(self, kind: &[u8], body: &[u8]) -> Vec<u8> {
        let mut object = Vec::with_capacity(body.len() + 32);
        object.extend_from_slice(kind);
        object.push(b' ');
        object.extend_from_slice(body.len().to_string().as_bytes());
        object.push(0);
        object.extend_from_slice(body);
        self.digest(&object)
    }

    fn oid_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }

    fn digest(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => sha1::Sha1::digest(bytes).to_vec(),
            Self::Sha256 => sha2::Sha256::digest(bytes).to_vec(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexTime {
    pub seconds: u32,
    pub nanoseconds: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexStat {
    pub ctime: IndexTime,
    pub mtime: IndexTime,
    pub dev: u32,
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

/// Full-width lstat fields supplied by the caller; times are Unix seconds.
/// On Windows ctime here means Git's creation time, not change-sensitive evidence.
#[derive(Clone, Debug)]
pub struct WorktreeStat {
    pub ctime_seconds: i64,
    pub ctime_nanoseconds: u32,
    pub mtime_seconds: i64,
    pub mtime_nanoseconds: u32,
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub size: u64,
}

impl IndexStat {
    /// Compare the recorded tuple only, never infer clean content from it.
    /// Git stores low 32 bits, including for signed seconds. Git for Windows
    /// does not record device/inode, so those fields cannot participate there.
    pub fn matches_stat(&self, stat: &WorktreeStat) -> bool {
        let identity_matches =
            cfg!(windows) || (self.dev == stat.dev as u32 && self.ino == stat.ino as u32);
        identity_matches
            && self.size == stat.size as u32
            && self.mode == stat.mode
            && self.ctime.seconds == stat.ctime_seconds as u32
            && self.ctime.nanoseconds == stat.ctime_nanoseconds
            && self.mtime.seconds == stat.mtime_seconds as u32
            && self.mtime.nanoseconds == stat.mtime_nanoseconds
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryException {
    Unmerged(u8),
    AssumeValid,
    SkipWorktree,
    IntentToAdd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// Git paths are bytes, not necessarily UTF-8, and use '/' on every platform.
    pub path: Vec<u8>,
    pub oid: Vec<u8>,
    pub stat: IndexStat,
    pub stage: u8,
    pub assume_valid: bool,
    pub skip_worktree: bool,
    pub intent_to_add: bool,
}

impl IndexEntry {
    pub fn exceptions(&self) -> Vec<EntryException> {
        let mut exceptions = Vec::new();
        if self.stage != 0 {
            exceptions.push(EntryException::Unmerged(self.stage));
        }
        if self.assume_valid {
            exceptions.push(EntryException::AssumeValid);
        }
        if self.skip_worktree {
            exceptions.push(EntryException::SkipWorktree);
        }
        if self.intent_to_add {
            exceptions.push(EntryException::IntentToAdd);
        }
        exceptions
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSnapshot {
    pub version: u32,
    pub object_format: ObjectFormat,
    pub entries: Vec<IndexEntry>,
    /// The verified trailer also identifies the exact index bytes observed.
    pub checksum: Vec<u8>,
    pub modified: SystemTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StrictReason {
    IndexLocked,
    Unreadable(io::ErrorKind),
    Unstable,
    ChecksumMismatch,
    UnsupportedVersion(u32),
    RequiredExtension([u8; 4]),
    SparseDirectory,
    Malformed(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexRead {
    Snapshot(IndexSnapshot),
    Strict(StrictReason),
}

#[derive(Clone, Copy, Debug)]
pub struct ReadOptions {
    pub attempts: usize,
    /// Total budget for waiting on index.lock, shared by all attempts.
    pub lock_wait: Duration,
    pub retry_delay: Duration,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            attempts: 3,
            lock_wait: Duration::from_millis(100),
            retry_delay: Duration::from_millis(5),
        }
    }
}

/// Read the explicitly supplied index (including linked-worktree/custom indexes).
/// The caller supplies the repository's object format; no Git discovery or writes
/// are hidden in a snapshot read. Strict requires an independent worktree scan
/// rather than using the index to avoid content reads.
pub fn read_index(path: &Path, format: ObjectFormat, options: ReadOptions) -> IndexRead {
    read_index_with_hook(path, format, options, || {})
}

fn read_index_with_hook(
    path: &Path,
    format: ObjectFormat,
    options: ReadOptions,
    mut after_read: impl FnMut(),
) -> IndexRead {
    let mut lock_name = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    let mut lock_remaining = options.lock_wait;
    let mut reason = StrictReason::Unstable;
    for attempt in 0..options.attempts {
        if let Err(reason) = wait_for_unlock(&lock_path, &mut lock_remaining) {
            return IndexRead::Strict(reason);
        }
        let result = read_once(path, format, &mut after_read);
        // A lock appearing during the read invalidates even checksum-valid bytes.
        let saw_lock = match wait_for_unlock(&lock_path, &mut lock_remaining) {
            Ok(saw_lock) => saw_lock,
            Err(reason) => return IndexRead::Strict(reason),
        };
        match if saw_lock {
            Err(StrictReason::Unstable)
        } else {
            result
        } {
            Ok(snapshot) => {
                // If the writer replaced the pathname while we held the old open
                // file, read_once rejects it rather than publishing the old index.
                return IndexRead::Snapshot(snapshot);
            }
            Err(error) => {
                let retry = matches!(
                    error,
                    StrictReason::Unstable
                        | StrictReason::ChecksumMismatch
                        | StrictReason::Unreadable(_)
                );
                reason = error;
                if !retry {
                    break;
                }
            }
        }
        if attempt + 1 < options.attempts {
            thread::sleep(options.retry_delay);
        }
    }
    IndexRead::Strict(reason)
}

fn wait_for_unlock(path: &Path, remaining: &mut Duration) -> Result<bool, StrictReason> {
    let start = Instant::now();
    let mut saw_lock = false;
    loop {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                *remaining = remaining.saturating_sub(start.elapsed());
                return Ok(saw_lock);
            }
            Err(error) => return Err(StrictReason::Unreadable(error.kind())),
            Ok(_) => saw_lock = true,
        }
        if start.elapsed() >= *remaining {
            *remaining = Duration::ZERO;
            return Err(StrictReason::IndexLocked);
        }
        thread::sleep(Duration::from_millis(5).min(*remaining - start.elapsed().min(*remaining)));
    }
}

fn read_once(
    path: &Path,
    format: ObjectFormat,
    after_read: &mut impl FnMut(),
) -> Result<IndexSnapshot, StrictReason> {
    let io_reason = |error: io::Error| StrictReason::Unreadable(error.kind());
    let mut file = File::open(path).map_err(io_reason)?;
    let before = file.metadata().map_err(io_reason)?;
    let before_identity = FileIdentity::of_file(&file);
    if !before.is_file() {
        return Err(StrictReason::Malformed("index is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(io_reason)?;
    after_read();
    let after = file.metadata().map_err(io_reason)?;
    let after_identity = FileIdentity::of_file(&file);
    let (current, current_identity) = crate::file_identity::metadata(path).map_err(io_reason)?;
    if !same_snapshot(&before, before_identity, &after, after_identity)
        || !same_snapshot(&after, after_identity, &current, current_identity)
        || before.len() != bytes.len() as u64
    {
        return Err(StrictReason::Unstable);
    }
    parse_index(&bytes, format, before.modified().map_err(io_reason)?)
}

fn same_snapshot(
    before: &Metadata,
    before_identity: FileIdentity,
    after: &Metadata,
    after_identity: FileIdentity,
) -> bool {
    before.len() == after.len()
        && before.modified().ok().is_some()
        && before.modified().ok() == after.modified().ok()
        && before_identity != FileIdentity::Unavailable
        && before_identity == after_identity
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], StrictReason> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(StrictReason::Malformed("length overflow"))?;
        let bytes = self
            .bytes
            .get(self.pos..end)
            .ok_or(StrictReason::Malformed("truncated index"))?;
        self.pos = end;
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32, StrictReason> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u16(&mut self) -> Result<u16, StrictReason> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn nul_string(&mut self) -> Result<&'a [u8], StrictReason> {
        let len = self.bytes[self.pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or(StrictReason::Malformed("unterminated path"))?;
        let string = self.take(len)?;
        self.take(1)?;
        Ok(string)
    }

    fn strip_count(&mut self) -> Result<usize, StrictReason> {
        let mut byte = self.take(1)?[0];
        let mut count = usize::from(byte & 0x7f);
        while byte & 0x80 != 0 {
            byte = self.take(1)?[0];
            count = count
                .checked_add(1)
                .and_then(|n| n.checked_mul(128))
                .and_then(|n| n.checked_add(usize::from(byte & 0x7f)))
                .ok_or(StrictReason::Malformed("v4 prefix overflow"))?;
        }
        Ok(count)
    }
}

fn parse_index(
    bytes: &[u8],
    format: ObjectFormat,
    modified: SystemTime,
) -> Result<IndexSnapshot, StrictReason> {
    let trailer = format.oid_len();
    let body_len = bytes
        .len()
        .checked_sub(trailer)
        .filter(|&n| n >= 12)
        .ok_or(StrictReason::ChecksumMismatch)?;
    let (body, checksum) = bytes.split_at(body_len);
    if format.digest(body) != checksum {
        return Err(StrictReason::ChecksumMismatch);
    }
    let mut cursor = Cursor {
        bytes: body,
        pos: 0,
    };
    if cursor.take(4)? != b"DIRC" {
        return Err(StrictReason::Malformed("invalid signature"));
    }
    let version = cursor.u32()?;
    if !(2..=4).contains(&version) {
        return Err(StrictReason::UnsupportedVersion(version));
    }
    let count = cursor.u32()?;
    let mut entries: Vec<IndexEntry> = Vec::new();
    for _ in 0..count {
        let start = cursor.pos;
        let stat = IndexStat {
            ctime: IndexTime {
                seconds: cursor.u32()?,
                nanoseconds: cursor.u32()?,
            },
            mtime: IndexTime {
                seconds: cursor.u32()?,
                nanoseconds: cursor.u32()?,
            },
            dev: cursor.u32()?,
            ino: cursor.u32()?,
            mode: cursor.u32()?,
            uid: cursor.u32()?,
            gid: cursor.u32()?,
            size: cursor.u32()?,
        };
        if stat.ctime.nanoseconds >= 1_000_000_000 || stat.mtime.nanoseconds >= 1_000_000_000 {
            return Err(StrictReason::Malformed("invalid stat nanoseconds"));
        }
        let oid = cursor.take(trailer)?.to_vec();
        let flags = cursor.u16()?;
        let extended = if flags & 0x4000 != 0 {
            if version == 2 {
                return Err(StrictReason::Malformed("extended flags in v2"));
            }
            let extended = cursor.u16()?;
            if extended & !0x6000 != 0 {
                return Err(StrictReason::Malformed("unknown extended entry flags"));
            }
            extended
        } else {
            0
        };
        let path = if version == 4 {
            let strip = cursor.strip_count()?;
            let previous = entries.last().map_or(&[][..], |entry| &entry.path);
            let prefix_len = previous
                .len()
                .checked_sub(strip)
                .ok_or(StrictReason::Malformed("v4 prefix exceeds previous path"))?;
            let mut path = previous[..prefix_len].to_vec();
            path.extend_from_slice(cursor.nul_string()?);
            path
        } else {
            let path = cursor.nul_string()?.to_vec();
            let padding = (8 - (cursor.pos - start) % 8) % 8;
            if cursor.take(padding)?.iter().any(|&byte| byte != 0) {
                return Err(StrictReason::Malformed("nonzero entry padding"));
            }
            path
        };
        if usize::from(flags & 0x0fff) != path.len().min(0x0fff) {
            return Err(StrictReason::Malformed("entry path length mismatch"));
        }
        entries.push(IndexEntry {
            path,
            oid,
            stat,
            stage: ((flags >> 12) & 3) as u8,
            assume_valid: flags & 0x8000 != 0,
            skip_worktree: extended & 0x4000 != 0,
            intent_to_add: extended & 0x2000 != 0,
        });
    }
    while cursor.pos < body.len() {
        let signature: [u8; 4] = cursor.take(4)?.try_into().unwrap();
        let len = cursor.u32()? as usize;
        cursor.take(len)?;
        if !signature[0].is_ascii_uppercase() {
            return Err(StrictReason::RequiredExtension(signature));
        }
    }
    // Split indexes can contain empty names; inspect extensions before validating
    // paths so their mandatory 'link' extension is reported as the strict reason.
    for (i, entry) in entries.iter().enumerate() {
        if entry.stat.mode & 0o170000 == 0o040000 {
            return Err(StrictReason::SparseDirectory);
        }
        if entry.path.is_empty()
            || entry
                .path
                .split(|&b| b == b'/')
                .any(|p| p.is_empty() || p == b"." || p == b"..")
        {
            return Err(StrictReason::Malformed("invalid repository-relative path"));
        }
        if i > 0 {
            let previous = &entries[i - 1];
            if (&previous.path, previous.stage) >= (&entry.path, entry.stage) {
                return Err(StrictReason::Malformed("entries out of order"));
            }
        }
    }
    Ok(IndexSnapshot {
        version,
        object_format: format,
        entries,
        checksum: checksum.to_vec(),
        modified,
    })
}

/// Why a coherent index snapshot cannot prove a full tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeRefusal {
    /// An unmerged, assume-valid, skip-worktree or intent-to-add entry.
    Exception(Vec<u8>, EntryException),
    /// A mode other than regular, executable, symlink or gitlink.
    Mode(Vec<u8>, u32),
    /// An empty, `.`, `..` or `.git` component.
    Path(Vec<u8>),
    /// The same path twice, or a path that is both a file and a directory.
    Conflict(Vec<u8>),
}

enum TreeNode<'a> {
    Leaf { mode: u32, oid: &'a [u8] },
    Directory(std::collections::BTreeMap<&'a [u8], TreeNode<'a>>),
}

/// Computes the root tree object ID that `git write-tree` would write for
/// this index, from every entry. Any entry that a plain tree cannot
/// represent exactly refuses the whole computation rather than being skipped,
/// so an equal result proves the full index, not a subset of it.
pub fn index_tree(snapshot: &IndexSnapshot) -> Result<Vec<u8>, TreeRefusal> {
    let mut root = std::collections::BTreeMap::new();
    for entry in &snapshot.entries {
        if let Some(exception) = entry.exceptions().into_iter().next() {
            return Err(TreeRefusal::Exception(entry.path.clone(), exception));
        }
        if !matches!(entry.stat.mode, 0o100644 | 0o100755 | 0o120000 | 0o160000) {
            return Err(TreeRefusal::Mode(entry.path.clone(), entry.stat.mode));
        }
        let components: Vec<&[u8]> = entry.path.split(|&b| b == b'/').collect();
        if components.iter().any(|component| {
            component.is_empty()
                || *component == b"."
                || *component == b".."
                || component.eq_ignore_ascii_case(b".git")
        }) {
            return Err(TreeRefusal::Path(entry.path.clone()));
        }
        let conflict = || TreeRefusal::Conflict(entry.path.clone());
        let (leaf, parents) = components.split_last().expect("split yields one item");
        let mut directory = &mut root;
        for parent in parents {
            let node = directory
                .entry(*parent)
                .or_insert_with(|| TreeNode::Directory(Default::default()));
            directory = match node {
                TreeNode::Directory(children) => children,
                TreeNode::Leaf { .. } => return Err(conflict()),
            };
        }
        if directory
            .insert(
                leaf,
                TreeNode::Leaf {
                    mode: entry.stat.mode,
                    oid: &entry.oid,
                },
            )
            .is_some()
        {
            return Err(conflict());
        }
    }
    Ok(write_tree(snapshot.object_format, &root))
}

fn write_tree(
    format: ObjectFormat,
    children: &std::collections::BTreeMap<&[u8], TreeNode<'_>>,
) -> Vec<u8> {
    // Git orders tree entries by name, comparing a directory as if its name
    // ended in '/'.
    let mut rows: Vec<(Vec<u8>, String, &[u8], Vec<u8>)> = children
        .iter()
        .map(|(name, node)| match node {
            TreeNode::Leaf { mode, oid } => {
                (name.to_vec(), format!("{mode:o}"), *name, oid.to_vec())
            }
            TreeNode::Directory(grandchildren) => {
                let mut key = name.to_vec();
                key.push(b'/');
                (
                    key,
                    "40000".to_owned(),
                    *name,
                    write_tree(format, grandchildren),
                )
            }
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut body = Vec::new();
    for (_, mode, name, oid) in rows {
        body.extend_from_slice(mode.as_bytes());
        body.push(b' ');
        body.extend_from_slice(name);
        body.push(0);
        body.extend_from_slice(&oid);
    }
    format.object_oid(b"tree", &body)
}

/// Git metadata queries for one snapshot evaluation. Every spawned Git child, including a
/// failed or timed-out command, increments the counter. Output remains binary
/// because the NUL-delimited protocols must preserve arbitrary Git path bytes.
/// Callers must treat unsuccessful exit status as strict, not as empty evidence.
pub struct GitQueries {
    root: PathBuf,
    timeout: Duration,
    spawned: AtomicU64,
}

impl GitQueries {
    pub fn new(root: PathBuf, timeout: Duration) -> Self {
        Self {
            root,
            timeout,
            spawned: AtomicU64::new(0),
        }
    }

    pub fn spawned(&self) -> u64 {
        self.spawned.load(Ordering::Relaxed)
    }

    pub fn head_tree(&self) -> io::Result<Output> {
        self.run(&["rev-parse", "HEAD^{tree}"], Vec::new())
    }

    pub fn diff_trees(&self, old: &str, new: &str) -> io::Result<Output> {
        // Only object IDs, never revision expressions or command-line options.
        let valid =
            |oid: &str| matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid(old) || !valid(new) || old.len() != new.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid tree OID",
            ));
        }
        self.run(
            &["diff-tree", "-r", "-z", "--no-renames", old, new, "--"],
            Vec::new(),
        )
    }

    pub fn check_attributes(&self, paths: &[&[u8]]) -> io::Result<Output> {
        let mut input = Vec::new();
        for path in paths {
            if path.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "NUL in Git path",
                ));
            }
            input.extend_from_slice(path);
            input.push(0);
        }
        self.run(
            &[
                "check-attr",
                "--stdin",
                "-z",
                "filter",
                "text",
                "eol",
                "working-tree-encoding",
                "ident",
            ],
            input,
        )
    }

    /// Return config values with their origins for the caller to interpret and
    /// retain. Callers should issue this query only once per snapshot evaluation.
    pub fn config(&self) -> io::Result<Output> {
        self.run(&["config", "--list", "--show-origin", "-z"], Vec::new())
    }
    /// Ask Git for its compiled/default attribute path rather than guessing an
    /// installation prefix. Older Git versions may refuse this variable.
    pub fn attribute_source(&self, global: bool) -> io::Result<Output> {
        self.run(
            &[
                "var",
                if global {
                    "GIT_ATTR_GLOBAL"
                } else {
                    "GIT_ATTR_SYSTEM"
                },
            ],
            Vec::new(),
        )
    }

    fn run(&self, args: &[&str], input: Vec<u8>) -> io::Result<Output> {
        let mut command = Command::new("git");
        command
            .current_dir(&self.root)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C");
        #[cfg(test)]
        crate::test_env::apply_hermetic_git_env(&mut command);
        pump(&mut command, input, self.timeout, &self.spawned)
    }
}

enum PumpResult {
    Input(io::Result<()>),
    Stdout(io::Result<Vec<u8>>),
    Stderr(io::Result<Vec<u8>>),
}

fn pump(
    command: &mut Command,
    input: Vec<u8>,
    timeout: Duration,
    spawned: &AtomicU64,
) -> io::Result<Output> {
    crate::format::isolate_in_process_group(command);
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    spawned.fetch_add(1, Ordering::Relaxed);
    crate::views::io::WORK
        .git_subprocesses
        .fetch_add(1, Ordering::Relaxed);
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel();
    let input_tx = tx.clone();
    thread::spawn(move || {
        let result = stdin.write_all(&input);
        drop(stdin);
        let _ = input_tx.send(PumpResult::Input(result));
    });
    let stdout_tx = tx.clone();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = stdout_tx.send(PumpResult::Stdout(result));
    });
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr.read_to_end(&mut bytes).map(|_| bytes);
        let _ = tx.send(PumpResult::Stderr(result));
    });
    let mut status = None;
    let mut input_done = false;
    let mut output = None;
    let mut error_output = None;
    let result = (|| loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Git metadata deadline exceeded",
            ));
        }
        if status.is_none() {
            status = child.try_wait()?;
        }
        if status.is_some() && input_done && output.is_some() && error_output.is_some() {
            return Ok(Output {
                status: status.unwrap(),
                stdout: output.take().unwrap(),
                stderr: error_output.take().unwrap(),
            });
        }
        // Completion includes EOF on both pipes and the stdin writer, not just
        // process exit. A descendant retaining a pipe must not defeat the deadline.
        match rx.recv_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(5)),
        ) {
            Ok(PumpResult::Input(result)) => {
                result?;
                input_done = true;
            }
            Ok(PumpResult::Stdout(result)) => output = Some(result?),
            Ok(PumpResult::Stderr(result)) => error_output = Some(result?),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if !input_done || output.is_none() || error_output.is_none() {
                    return Err(io::Error::other("Git pipe pump disconnected"));
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    })();
    if result.is_err() {
        crate::format::kill_process_tree(&mut child);
        let _ = child.wait();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git(root: &Path, args: &[&str], input: &[u8]) -> Output {
        let mut command = Command::new("git");
        command
            .current_dir(root)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0");
        crate::test_env::apply_hermetic_git_env(&mut command);
        let output = pump(
            &mut command,
            input.to_vec(),
            Duration::from_secs(15),
            &AtomicU64::new(0),
        )
        .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn fixture(format: ObjectFormat) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(
            dir.path(),
            &[
                "init",
                if format == ObjectFormat::Sha1 {
                    "--object-format=sha1"
                } else {
                    "--object-format=sha256"
                },
            ],
            &[],
        );
        git(
            dir.path(),
            &["config", "user.email", "index-reader@example.invalid"],
            &[],
        );
        git(dir.path(), &["config", "user.name", "Index Reader"], &[]);
        git(dir.path(), &["config", "core.autocrlf", "false"], &[]);
        git(dir.path(), &["config", "core.splitIndex", "false"], &[]);
        for path in [
            "plain",
            "skip",
            "dir/sub/alpha",
            "dir/sub/alphabet",
            "with space",
        ] {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"hello\n").unwrap();
        }
        // A long first path followed by a short unrelated one exercises the
        // multi-byte remove-count encoding in Git's real v4 prefix compressor.
        fs::write(dir.path().join("a".repeat(140)), b"long\n").unwrap();
        git(dir.path(), &["add", "."], &[]);
        git(
            dir.path(),
            &["-c", "commit.gpgsign=false", "commit", "-m", "fixture"],
            &[],
        );
        dir
    }

    fn index(root: &Path) -> PathBuf {
        root.join(".git/index")
    }

    fn read(root: &Path, format: ObjectFormat) -> IndexRead {
        read_index(&index(root), format, ReadOptions::default())
    }

    fn snapshot(root: &Path, format: ObjectFormat) -> IndexSnapshot {
        match read(root, format) {
            IndexRead::Snapshot(snapshot) => snapshot,
            other => panic!("expected snapshot: {other:?}"),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn assert_matches_git(root: &Path, snapshot: &IndexSnapshot) {
        let mut actual = Vec::new();
        for entry in &snapshot.entries {
            actual.extend_from_slice(
                format!(
                    "{:06o} {} {}\t",
                    entry.stat.mode,
                    hex(&entry.oid),
                    entry.stage
                )
                .as_bytes(),
            );
            actual.extend_from_slice(&entry.path);
            actual.push(0);
        }
        assert_eq!(
            actual,
            git(root, &["ls-files", "--stage", "-z"], &[]).stdout
        );
        let plain = snapshot
            .entries
            .iter()
            .find(|e| e.path == b"plain")
            .unwrap();
        assert_eq!(plain.stat.size, 6);
        assert_eq!(plain.stat.mode, 0o100644);
        assert_eq!(snapshot.entries.len(), 6);
    }

    #[test]
    fn real_git_v2_golden() {
        let dir = fixture(ObjectFormat::Sha1);
        git(
            dir.path(),
            &["update-index", "--assume-unchanged", "plain"],
            &[],
        );
        git(dir.path(), &["update-index", "--index-version", "2"], &[]);
        let snapshot = snapshot(dir.path(), ObjectFormat::Sha1);
        assert_eq!(snapshot.version, 2);
        assert_matches_git(dir.path(), &snapshot);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .find(|e| e.path == b"plain")
                .unwrap()
                .exceptions(),
            vec![EntryException::AssumeValid]
        );
    }

    #[test]
    fn real_git_v3_golden_and_extended_flags() {
        let dir = fixture(ObjectFormat::Sha1);
        git(
            dir.path(),
            &["update-index", "--skip-worktree", "skip"],
            &[],
        );
        fs::write(dir.path().join("intent"), b"not staged\n").unwrap();
        git(dir.path(), &["add", "--intent-to-add", "intent"], &[]);
        git(dir.path(), &["update-index", "--index-version", "3"], &[]);
        let snapshot = snapshot(dir.path(), ObjectFormat::Sha1);
        assert_eq!(snapshot.version, 3);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .find(|e| e.path == b"skip")
                .unwrap()
                .exceptions(),
            vec![EntryException::SkipWorktree]
        );
        assert_eq!(
            snapshot
                .entries
                .iter()
                .find(|e| e.path == b"intent")
                .unwrap()
                .exceptions(),
            vec![EntryException::IntentToAdd]
        );
        assert_eq!(snapshot.entries.len(), 7);
    }

    #[test]
    fn real_git_v4_golden_prefix_compression() {
        let dir = fixture(ObjectFormat::Sha1);
        git(dir.path(), &["update-index", "--index-version", "4"], &[]);
        let snapshot = snapshot(dir.path(), ObjectFormat::Sha1);
        assert_eq!(snapshot.version, 4);
        assert_matches_git(dir.path(), &snapshot);
        assert_eq!(snapshot.entries[0].path, vec![b'a'; 140]);
        assert_eq!(snapshot.entries[1].path, b"dir/sub/alpha");
        assert_eq!(snapshot.entries[2].path, b"dir/sub/alphabet");
    }

    #[test]
    fn real_git_sha256_golden_all_versions() {
        let dir = fixture(ObjectFormat::Sha256);
        for version in ["2", "3", "4"] {
            if version == "3" {
                git(
                    dir.path(),
                    &["update-index", "--skip-worktree", "skip"],
                    &[],
                );
            }
            git(
                dir.path(),
                &["update-index", "--index-version", version],
                &[],
            );
            let snapshot = snapshot(dir.path(), ObjectFormat::Sha256);
            assert_eq!(snapshot.version.to_string(), version);
            assert_eq!(snapshot.checksum.len(), 32);
            assert!(snapshot.entries.iter().all(|e| e.oid.len() == 32));
            assert_matches_git(dir.path(), &snapshot);
        }
        assert_eq!(
            read(dir.path(), ObjectFormat::Sha1),
            IndexRead::Strict(StrictReason::ChecksumMismatch)
        );
    }

    #[test]
    fn real_git_unmerged_stages_are_exceptions() {
        let dir = fixture(ObjectFormat::Sha1);
        let oid = git(dir.path(), &["rev-parse", "HEAD:plain"], &[]).stdout;
        let oid = String::from_utf8(oid).unwrap();
        let records: String = (1..=3)
            .map(|stage| format!("100644 {} {stage}\tconflict\n", oid.trim()))
            .collect();
        git(
            dir.path(),
            &["update-index", "--index-info"],
            records.as_bytes(),
        );
        let snapshot = snapshot(dir.path(), ObjectFormat::Sha1);
        let stages: Vec<_> = snapshot
            .entries
            .iter()
            .filter(|e| e.path == b"conflict")
            .map(|e| e.exceptions())
            .collect();
        assert_eq!(
            stages,
            vec![
                vec![EntryException::Unmerged(1)],
                vec![EntryException::Unmerged(2)],
                vec![EntryException::Unmerged(3)]
            ]
        );
    }

    #[test]
    fn real_git_split_index_is_strict() {
        let dir = fixture(ObjectFormat::Sha1);
        git(dir.path(), &["update-index", "--split-index"], &[]);
        assert_eq!(
            read(dir.path(), ObjectFormat::Sha1),
            IndexRead::Strict(StrictReason::RequiredExtension(*b"link"))
        );
    }

    #[test]
    fn real_git_sparse_index_is_strict() {
        let dir = fixture(ObjectFormat::Sha1);
        git(
            dir.path(),
            &["sparse-checkout", "init", "--cone", "--sparse-index"],
            &[],
        );
        git(
            dir.path(),
            &["sparse-checkout", "set", "--sparse-index", "other"],
            &[],
        );
        assert_eq!(
            read(dir.path(), ObjectFormat::Sha1),
            IndexRead::Strict(StrictReason::RequiredExtension(*b"sdir"))
        );
    }

    #[test]
    fn held_index_lock_waits_then_strict_without_touching_it() {
        let dir = fixture(ObjectFormat::Sha1);
        let lock = dir.path().join(".git/index.lock");
        fs::write(&lock, b"writer owns this").unwrap();
        let options = ReadOptions {
            lock_wait: Duration::from_millis(20),
            ..ReadOptions::default()
        };
        let start = Instant::now();
        assert_eq!(
            read_index(&index(dir.path()), ObjectFormat::Sha1, options),
            IndexRead::Strict(StrictReason::IndexLocked)
        );
        assert!(start.elapsed() >= options.lock_wait);
        assert_eq!(fs::read(lock).unwrap(), b"writer owns this");
    }

    #[test]
    fn released_index_lock_allows_snapshot() {
        let dir = fixture(ObjectFormat::Sha1);
        let lock = dir.path().join(".git/index.lock");
        fs::write(&lock, b"writer").unwrap();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            fs::remove_file(lock).unwrap();
        });
        let result = read_index(
            &index(dir.path()),
            ObjectFormat::Sha1,
            ReadOptions {
                lock_wait: Duration::from_secs(5),
                ..ReadOptions::default()
            },
        );
        writer.join().unwrap();
        assert!(matches!(result, IndexRead::Snapshot(_)));
    }

    #[test]
    fn real_index_torn_read_retries_then_recovers() {
        let dir = fixture(ObjectFormat::Sha1);
        let path = index(dir.path());
        let original = fs::read(&path).unwrap();
        let mut attempts = 0;
        let result =
            read_index_with_hook(&path, ObjectFormat::Sha1, ReadOptions::default(), || {
                attempts += 1;
                match attempts {
                    1 => fs::write(&path, &original[..original.len() / 2]).unwrap(),
                    2 => fs::write(&path, &original).unwrap(),
                    _ => {}
                }
            });
        assert_eq!(attempts, 3);
        assert!(matches!(result, IndexRead::Snapshot(_)));
    }

    #[test]
    fn real_index_persistently_torn_read_is_strict() {
        let dir = fixture(ObjectFormat::Sha1);
        let path = index(dir.path());
        let original = fs::read(&path).unwrap();
        let mut attempts = 0;
        let result =
            read_index_with_hook(&path, ObjectFormat::Sha1, ReadOptions::default(), || {
                attempts += 1;
                fs::write(
                    &path,
                    if attempts % 2 == 0 {
                        &original[..]
                    } else {
                        &original[..original.len() / 2]
                    },
                )
                .unwrap();
            });
        assert_eq!(attempts, 3);
        assert_eq!(result, IndexRead::Strict(StrictReason::Unstable));
    }

    #[test]
    fn checksum_mismatch_retries_then_strict() {
        let dir = fixture(ObjectFormat::Sha1);
        let path = index(dir.path());
        let mut bytes = fs::read(&path).unwrap();
        bytes[20] ^= 1;
        fs::write(&path, bytes).unwrap();
        let mut attempts = 0;
        let result =
            read_index_with_hook(&path, ObjectFormat::Sha1, ReadOptions::default(), || {
                attempts += 1
            });
        assert_eq!(attempts, 3);
        assert_eq!(result, IndexRead::Strict(StrictReason::ChecksumMismatch));
    }

    #[test]
    fn replacing_index_with_same_size_and_mtime_is_not_coherent() {
        let dir = fixture(ObjectFormat::Sha1);
        let path = index(dir.path());
        let original = fs::read(&path).unwrap();
        let mtime = filetime::FileTime::from_last_modification_time(&fs::metadata(&path).unwrap());
        let result = read_index_with_hook(
            &path,
            ObjectFormat::Sha1,
            ReadOptions {
                attempts: 1,
                ..ReadOptions::default()
            },
            || {
                let replacement = path.with_extension("replacement");
                fs::write(&replacement, &original).unwrap();
                filetime::set_file_mtime(&replacement, mtime).unwrap();
                // Windows cannot rename over an existing file. Moving the old name
                // first keeps the original open handle alive on all three platforms.
                fs::rename(&path, path.with_extension("old")).unwrap();
                fs::rename(replacement, &path).unwrap();
            },
        );
        assert_eq!(result, IndexRead::Strict(StrictReason::Unstable));
    }

    #[test]
    fn same_size_in_place_mtime_change_is_not_coherent() {
        let dir = fixture(ObjectFormat::Sha1);
        let path = index(dir.path());
        let result = read_index_with_hook(
            &path,
            ObjectFormat::Sha1,
            ReadOptions {
                attempts: 1,
                ..ReadOptions::default()
            },
            || {
                filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1, 123))
                    .unwrap();
            },
        );
        assert_eq!(result, IndexRead::Strict(StrictReason::Unstable));
    }

    fn append_extension(bytes: &[u8], signature: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut body = bytes[..bytes.len() - 20].to_vec();
        body.extend_from_slice(signature);
        body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        body.extend_from_slice(payload);
        let digest = ObjectFormat::Sha1.digest(&body);
        body.extend_from_slice(&digest);
        body
    }

    #[test]
    fn optional_extensions_are_ignored_required_extensions_are_strict() {
        let dir = fixture(ObjectFormat::Sha1);
        let path = index(dir.path());
        let original = fs::read(&path).unwrap();
        let expected = snapshot(dir.path(), ObjectFormat::Sha1).entries;
        for signature in [b"ABCD", b"Z123"] {
            fs::write(
                &path,
                append_extension(&original, signature, b"optional payload"),
            )
            .unwrap();
            assert_eq!(snapshot(dir.path(), ObjectFormat::Sha1).entries, expected);
        }
        for signature in [b"abcd", b"0123", b"link", b"sdir"] {
            fs::write(
                &path,
                append_extension(&original, signature, b"required payload"),
            )
            .unwrap();
            assert_eq!(
                read(dir.path(), ObjectFormat::Sha1),
                IndexRead::Strict(StrictReason::RequiredExtension(*signature))
            );
        }
    }

    fn seal(mut body: Vec<u8>) -> Vec<u8> {
        body.extend_from_slice(&ObjectFormat::Sha1.digest(&body));
        body
    }

    #[test]
    fn malformed_entries_extensions_and_versions_fail_closed() {
        let dir = fixture(ObjectFormat::Sha1);
        git(dir.path(), &["update-index", "--index-version", "4"], &[]);
        let path = index(dir.path());
        let original = fs::read(&path).unwrap();
        let body = &original[..original.len() - 20];
        let mut invalid_prefix = body.to_vec();
        invalid_prefix[12 + 40 + 20 + 2] = 1;
        let mut invalid_version = body.to_vec();
        invalid_version[4..8].copy_from_slice(&5u32.to_be_bytes());
        let mut invalid_count = body.to_vec();
        invalid_count[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        let mut invalid_nanoseconds = body.to_vec();
        invalid_nanoseconds[16..20].copy_from_slice(&1_000_000_000u32.to_be_bytes());
        let mut truncated_extension = body.to_vec();
        truncated_extension.extend_from_slice(b"ABCD\0\0\0\x10short");
        for malformed in [
            invalid_prefix,
            invalid_version,
            invalid_count,
            invalid_nanoseconds,
            truncated_extension,
            body[..16].to_vec(),
        ] {
            fs::write(&path, seal(malformed)).unwrap();
            assert!(matches!(
                read(dir.path(), ObjectFormat::Sha1),
                IndexRead::Strict(_)
            ));
        }
    }

    #[test]
    fn stat_comparison_uses_low_32_bits_and_windows_unrecorded_identity() {
        let stat = IndexStat {
            ctime: IndexTime {
                seconds: u32::MAX,
                nanoseconds: 321,
            },
            mtime: IndexTime {
                seconds: 7,
                nanoseconds: 654,
            },
            dev: 3,
            ino: 4,
            mode: 0o100644,
            uid: 10,
            gid: 20,
            size: 5,
        };
        let mut observed = WorktreeStat {
            ctime_seconds: -1,
            ctime_nanoseconds: 321,
            mtime_seconds: (1i64 << 32) + 7,
            mtime_nanoseconds: 654,
            dev: (1u64 << 32) + 3,
            ino: (1u64 << 32) + 4,
            mode: 0o100644,
            size: (1u64 << 32) + 5,
        };
        assert!(stat.matches_stat(&observed));
        observed.size += 1;
        assert!(!stat.matches_stat(&observed));
        observed.size -= 1;
        observed.dev = 0;
        observed.ino = 0;
        assert_eq!(stat.matches_stat(&observed), cfg!(windows));
        observed.mtime_nanoseconds += 1;
        assert!(!stat.matches_stat(&observed));
    }

    #[cfg(unix)]
    #[test]
    fn real_git_preserves_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = fixture(ObjectFormat::Sha1);
        let name = b"non-utf8-\xff";
        fs::write(dir.path().join(OsStr::from_bytes(name)), b"raw\n").unwrap();
        git(dir.path(), &["add", "."], &[]);
        git(dir.path(), &["update-index", "--index-version", "4"], &[]);
        assert!(snapshot(dir.path(), ObjectFormat::Sha1)
            .entries
            .iter()
            .any(|e| e.path == name));
    }

    fn trimmed_hex(output: Output) -> String {
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn index_tree_and_blob_oids_match_git_for_both_object_formats() {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let dir = fixture(format);
            // Names whose Git tree order differs from plain byte order once
            // the directory `a` is compared as `a/`.
            for path in ["order/a-b", "order/a/x", "order/a.b", "order/a0"] {
                let path = dir.path().join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                let contents = path_bytes(&path);
                fs::write(path, contents).unwrap();
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let tool = dir.path().join("tool.sh");
                fs::write(&tool, b"#!/bin/sh\n").unwrap();
                fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
                std::os::unix::fs::symlink("plain", dir.path().join("link")).unwrap();
            }
            git(dir.path(), &["add", "."], &[]);
            let commit = trimmed_hex(git(dir.path(), &["rev-parse", "HEAD"], &[]));
            git(
                dir.path(),
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{commit},module"),
                ],
                &[],
            );
            let snapshot = snapshot(dir.path(), format);
            assert_eq!(
                hex(&index_tree(&snapshot).unwrap()),
                trimmed_hex(git(dir.path(), &["write-tree"], &[])),
                "{format:?}"
            );
            let bytes = b"blob bytes\0with a NUL\n";
            assert_eq!(
                hex(&format.blob_oid(bytes)),
                trimmed_hex(git(dir.path(), &["hash-object", "--stdin"], bytes))
            );
            assert_eq!(
                ObjectFormat::from_hex_len(hex(&format.blob_oid(b"")).len()),
                Some(format)
            );
        }
    }

    fn path_bytes(path: &Path) -> Vec<u8> {
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .as_bytes()
            .to_vec()
    }

    #[test]
    fn index_tree_refuses_real_git_exception_entries() {
        let dir = fixture(ObjectFormat::Sha1);
        let tree = |dir: &TempDir| index_tree(&snapshot(dir.path(), ObjectFormat::Sha1));
        assert!(tree(&dir).is_ok());
        fs::write(dir.path().join("new"), b"new\n").unwrap();
        git(dir.path(), &["add", "-N", "new"], &[]);
        assert_eq!(
            tree(&dir),
            Err(TreeRefusal::Exception(
                b"new".to_vec(),
                EntryException::IntentToAdd
            ))
        );
        git(dir.path(), &["update-index", "--force-remove", "new"], &[]);
        git(
            dir.path(),
            &["update-index", "--skip-worktree", "skip"],
            &[],
        );
        assert_eq!(
            tree(&dir),
            Err(TreeRefusal::Exception(
                b"skip".to_vec(),
                EntryException::SkipWorktree
            ))
        );
        git(
            dir.path(),
            &["update-index", "--no-skip-worktree", "skip"],
            &[],
        );
        git(
            dir.path(),
            &["update-index", "--assume-unchanged", "plain"],
            &[],
        );
        assert_eq!(
            tree(&dir),
            Err(TreeRefusal::Exception(
                b"plain".to_vec(),
                EntryException::AssumeValid
            ))
        );
    }

    #[test]
    fn index_tree_refuses_modes_paths_and_conflicts_a_tree_cannot_hold() {
        let entry = |path: &[u8], mode: u32| IndexEntry {
            path: path.to_vec(),
            oid: vec![7; 20],
            stat: IndexStat {
                ctime: IndexTime {
                    seconds: 0,
                    nanoseconds: 0,
                },
                mtime: IndexTime {
                    seconds: 0,
                    nanoseconds: 0,
                },
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                size: 0,
            },
            stage: 0,
            assume_valid: false,
            skip_worktree: false,
            intent_to_add: false,
        };
        let tree = |entries: Vec<IndexEntry>| {
            index_tree(&IndexSnapshot {
                version: 2,
                object_format: ObjectFormat::Sha1,
                entries,
                checksum: Vec::new(),
                modified: SystemTime::UNIX_EPOCH,
            })
        };
        assert!(tree(vec![entry(b"a", 0o100644), entry(b"b/c", 0o120000)]).is_ok());
        assert_eq!(
            tree(vec![entry(b"a", 0o100664)]),
            Err(TreeRefusal::Mode(b"a".to_vec(), 0o100664))
        );
        assert_eq!(
            tree(vec![entry(b"x/.GIT/y", 0o100644)]),
            Err(TreeRefusal::Path(b"x/.GIT/y".to_vec()))
        );
        assert_eq!(
            tree(vec![entry(b"a", 0o100644), entry(b"a/b", 0o100644)]),
            Err(TreeRefusal::Conflict(b"a/b".to_vec()))
        );
        assert_eq!(
            tree(vec![entry(b"a/b", 0o100644), entry(b"a", 0o100644)]),
            Err(TreeRefusal::Conflict(b"a".to_vec()))
        );
        assert_eq!(
            tree(vec![entry(b"a", 0o100644), entry(b"a", 0o100755)]),
            Err(TreeRefusal::Conflict(b"a".to_vec()))
        );
    }

    #[test]
    fn counted_read_only_git_queries_and_large_bidirectional_pump() {
        let dir = fixture(ObjectFormat::Sha1);
        let path = index(dir.path());
        let original = fs::read(&path).unwrap();
        fs::write(
            dir.path().join(".gitattributes"),
            b"plain text eol=lf filter=demo\n",
        )
        .unwrap();
        let queries = GitQueries::new(dir.path().to_path_buf(), Duration::from_secs(15));
        let first = queries.head_tree().unwrap();
        assert!(first.status.success());
        let first = String::from_utf8(first.stdout).unwrap();
        git(dir.path(), &["rm", "with space"], &[]);
        git(
            dir.path(),
            &["-c", "commit.gpgsign=false", "commit", "-m", "second"],
            &[],
        );
        let second = queries.head_tree().unwrap();
        let second = String::from_utf8(second.stdout).unwrap();
        let after_commit = fs::read(&path).unwrap();
        let diff = queries.diff_trees(first.trim(), second.trim()).unwrap();
        assert!(diff.status.success());
        assert!(diff.stdout.ends_with(b"\0with space\0"));
        assert!(queries.diff_trees("--bad", second.trim()).is_err());
        // Both the request and reply exceed pipe capacity, so serial write/read
        // would deadlock. Repeating paths is valid check-attr input on every OS.
        let paths = vec![b"plain".as_slice(); 30_000];
        let attrs = queries.check_attributes(&paths).unwrap();
        assert!(
            attrs.status.success(),
            "{}",
            String::from_utf8_lossy(&attrs.stderr)
        );
        assert_eq!(attrs.stdout.len(), b"plain\0filter\0demo\0plain\0text\0set\0plain\0eol\0lf\0plain\0working-tree-encoding\0unspecified\0plain\0ident\0unspecified\0".len() * paths.len());
        assert!(attrs
            .stdout
            .starts_with(b"plain\0filter\0demo\0plain\0text\0set\0"));
        let config = queries.config().unwrap();
        assert!(config.status.success());
        assert!(config
            .stdout
            .windows(b"core.autocrlf\nfalse\0".len())
            .any(|w| w == b"core.autocrlf\nfalse\0"));
        assert_eq!(queries.spawned(), 5);
        assert_eq!(fs::read(&path).unwrap(), after_commit);
        assert!(!dir.path().join(".git/index.lock").exists());
        assert_ne!(original, after_commit);
    }

    #[test]
    fn git_pump_deadline_and_spawn_accounting() {
        // Re-exec this libtest binary with a private ignored sleeper, avoiding
        // shell syntax and utilities unavailable on native Windows runners.
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--ignored",
            "--exact",
            "git_index_reader::tests::pump_sleep_child",
            "--nocapture",
        ]);
        let spawned = AtomicU64::new(0);
        let start = Instant::now();
        let error = pump(
            &mut command,
            vec![b'x'; 1024 * 1024],
            Duration::from_millis(100),
            &spawned,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(spawned.load(Ordering::Relaxed), 1);
        assert!(start.elapsed() < Duration::from_secs(10));
        let mut missing = Command::new("aft-nonexistent-index-reader-test-command");
        assert!(pump(&mut missing, Vec::new(), Duration::from_secs(1), &spawned).is_err());
        assert_eq!(spawned.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[ignore = "subprocess helper; invoked explicitly by git_pump_deadline_and_spawn_accounting"]
    fn pump_sleep_child() {
        thread::sleep(Duration::from_secs(60));
    }
}
#[test]
fn attribute_source_queries_count_both_spawns_even_on_unsupported_git() {
    let root = tempfile::tempdir().unwrap();
    let queries = GitQueries::new(root.path().to_owned(), Duration::from_secs(2));
    let before = crate::views::io::WORK
        .git_subprocesses
        .load(Ordering::Relaxed);
    queries.attribute_source(false).unwrap();
    queries.attribute_source(true).unwrap();
    assert_eq!(queries.spawned(), 2);
    assert!(
        crate::views::io::WORK
            .git_subprocesses
            .load(Ordering::Relaxed)
            >= before + 2
    );
}
