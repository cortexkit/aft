use std::io;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

use super::persistence::{
    open_task_artifact_once, replace_artifact_with_tail, retry_artifact_read, TaskArtifact,
    TaskPaths, ValidatedArtifact,
};

#[cfg(test)]
static TAIL_READS: OnceLock<Mutex<std::collections::HashMap<PathBuf, usize>>> = OnceLock::new();

pub const DISK_LIMIT_BYTES: u64 = 100 * 1024 * 1024;

struct RecoveredReadDiagnostics {
    task_id: String,
    #[cfg(feature = "test-timing-hooks")]
    fail_stdout: bool,
    #[cfg(test)]
    messages: Vec<String>,
}

thread_local! {
    static RECOVERED_READ_DIAGNOSTICS: std::cell::RefCell<Option<RecoveredReadDiagnostics>> = const { std::cell::RefCell::new(None) };
}

/// Log reads of stored stdout/stderr while rebuilding a shell reply after restart.
/// Diagnostics leave the reply unchanged: read errors still follow the existing
/// empty-output fallback.
pub(crate) fn with_recovered_read_diagnostics<T>(task_id: &str, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<RecoveredReadDiagnostics>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RECOVERED_READ_DIAGNOSTICS.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let scope = RecoveredReadDiagnostics {
        task_id: task_id.to_string(),
        #[cfg(feature = "test-timing-hooks")]
        fail_stdout: std::env::var_os("AFT_TEST_RECOVERED_STDOUT_READ_FAILURE_FILE")
            .is_some_and(|path| Path::new(&path).exists()),
        #[cfg(test)]
        messages: Vec::new(),
    };
    let _restore = Restore(RECOVERED_READ_DIAGNOSTICS.with(|slot| slot.replace(Some(scope))));
    run()
}

#[derive(Debug, Clone, Copy)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedRead {
    pub text: String,
    pub truncated: bool,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskTruncation {
    pub stdout_prefix_bytes: u64,
    pub stderr_prefix_bytes: u64,
    pub combined_prefix_bytes: u64,
}

impl DiskTruncation {
    pub fn total_prefix_bytes(self) -> u64 {
        self.stdout_prefix_bytes
            .saturating_add(self.stderr_prefix_bytes)
            .saturating_add(self.combined_prefix_bytes)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ArtifactSource {
    Registered {
        paths: TaskPaths,
        artifact: TaskArtifact,
    },
    #[cfg(test)]
    Exact(PathBuf),
}

impl ArtifactSource {
    fn open(&self) -> io::Result<ValidatedArtifact> {
        retry_artifact_read(|| self.open_once())
    }

    fn open_once(&self) -> io::Result<ValidatedArtifact> {
        #[cfg(feature = "test-timing-hooks")]
        if matches!(
            self,
            Self::Registered {
                artifact: TaskArtifact::Stdout,
                ..
            }
        ) && RECOVERED_READ_DIAGNOSTICS.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|scope| scope.fail_stdout)
        }) {
            return self.record_result(
                "open",
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected recovered stdout read failure",
                )),
                |_| None,
            );
        }
        let result = match self {
            Self::Registered { paths, artifact } => open_task_artifact_once(paths, *artifact),
            #[cfg(test)]
            Self::Exact(path) => super::persistence::open_unregistered_artifact(path),
        };
        self.record_result("open", result, |_| None)
    }

    fn record_result<T>(
        &self,
        operation: &str,
        result: io::Result<T>,
        bytes: impl FnOnce(&T) -> Option<u64>,
    ) -> io::Result<T> {
        RECOVERED_READ_DIAGNOSTICS.with(|slot| {
            if let Some(scope) = slot.borrow_mut().as_mut() {
                let outcome = match &result {
                    Ok(value) => format!("ok bytes={:?}", bytes(value)),
                    Err(error) => format!("error kind={:?} detail={error}", error.kind()),
                };
                let message = format!("recovered artifact read task={} artifact={} operation={operation} result={outcome}", scope.task_id, self.path().display());
                crate::slog_info!("{message}");
                #[cfg(test)]
                scope.messages.push(message);
            }
        });
        result
    }

    fn len(&self, file: &ValidatedArtifact) -> io::Result<u64> {
        self.record_result("stat", file.len(), |len| Some(*len))
    }

    fn read_range(
        &self,
        file: &mut ValidatedArtifact,
        start: u64,
        len: u64,
    ) -> io::Result<Vec<u8>> {
        self.record_result("read_range", file.read_range(start, len), |bytes| {
            Some(bytes.len() as u64)
        })
    }

    fn sampled(&self) -> io::Result<(ValidatedArtifact, u64)> {
        let (file, len) = retry_artifact_read(|| {
            let file = self.open_once()?;
            let len = self.len(&file)?;
            Ok((file, len))
        })?;
        #[cfg(test)]
        GROW_AFTER_STAT.with(|hook| {
            if let Some(bytes) = hook.borrow_mut().take() {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(self.path())
                    .unwrap()
                    .write_all(&bytes)
                    .unwrap();
            }
        });
        Ok((file, len))
    }

    fn path(&self) -> &Path {
        match self {
            Self::Registered { paths, artifact } => paths.artifact_path(*artifact),
            #[cfg(test)]
            Self::Exact(path) => path,
        }
    }

    fn replace_with_tail(&self, retain_bytes: u64) -> io::Result<u64> {
        let result = match self {
            Self::Registered { paths, artifact } => {
                replace_artifact_with_tail(paths, *artifact, retain_bytes)
            }
            #[cfg(test)]
            Self::Exact(path) => {
                super::persistence::replace_unregistered_with_tail(path, retain_bytes)
            }
        };
        self.record_result("cap_removed_prefix", result, |bytes| Some(*bytes))
    }
}

#[cfg(test)]
thread_local! {
    static GROW_AFTER_STAT: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
}

#[derive(Debug, Clone)]
pub(crate) enum BgBuffer {
    Pipes {
        stdout: ArtifactSource,
        stderr: ArtifactSource,
    },
    Pty {
        combined: ArtifactSource,
    },
}

impl BgBuffer {
    pub fn registered(paths: &TaskPaths, mode: super::persistence::BgMode) -> Self {
        match mode {
            super::persistence::BgMode::Pipes => Self::Pipes {
                stdout: ArtifactSource::Registered {
                    paths: paths.clone(),
                    artifact: TaskArtifact::Stdout,
                },
                stderr: ArtifactSource::Registered {
                    paths: paths.clone(),
                    artifact: TaskArtifact::Stderr,
                },
            },
            super::persistence::BgMode::Pty => Self::Pty {
                combined: ArtifactSource::Registered {
                    paths: paths.clone(),
                    artifact: TaskArtifact::Pty,
                },
            },
        }
    }

    #[cfg(test)]
    pub fn new(stdout_path: PathBuf, stderr_path: PathBuf) -> Self {
        Self::Pipes {
            stdout: ArtifactSource::Exact(stdout_path),
            stderr: ArtifactSource::Exact(stderr_path),
        }
    }

    pub fn stderr_path(&self) -> Option<&Path> {
        match self {
            Self::Pipes { stderr, .. } => Some(stderr.path()),
            Self::Pty { .. } => None,
        }
    }

    pub fn read_tail(&self, max_bytes: usize) -> (String, bool) {
        #[cfg(test)]
        bump_tail_read_count(self);
        match self {
            Self::Pipes { stdout, stderr } => read_two_file_tails(stdout, stderr, max_bytes),
            Self::Pty { combined } => match read_source_tail(combined, max_bytes) {
                Ok((bytes, truncated)) => (String::from_utf8_lossy(&bytes).into_owned(), truncated),
                Err(_) => (String::new(), false),
            },
        }
    }

    pub fn read_combined_head_tail(
        &self,
        max_bytes: usize,
        head_bytes: usize,
        tail_bytes: usize,
    ) -> BoundedRead {
        self.read_combined_head_tail_with_boundary(max_bytes, head_bytes, tail_bytes)
            .0
    }

    /// Keep the stdout boundary for compression. The captures are concatenated,
    /// not chronological: stderr's last build message may precede stdout's
    /// final script verdict in real time.
    pub(crate) fn read_combined_head_tail_with_boundary(
        &self,
        max_bytes: usize,
        head_bytes: usize,
        tail_bytes: usize,
    ) -> (BoundedRead, usize) {
        match self {
            Self::Pipes { stdout, stderr } => {
                read_two_file_head_tail(stdout, stderr, max_bytes, head_bytes, tail_bytes)
            }
            Self::Pty { combined } => {
                let read = read_single_file_head_tail(combined, max_bytes, head_bytes, tail_bytes)
                    .unwrap_or_else(|_| empty_bounded_read());
                let boundary = read.text.len();
                (read, boundary)
            }
        }
    }

    pub fn read_stream_bounded(&self, stream: StreamKind, max_bytes: usize) -> BoundedRead {
        self.source(stream)
            .and_then(|source| read_source_bounded(source, max_bytes).ok())
            .unwrap_or_else(empty_bounded_read)
    }

    /// Notice redaction needs contiguous input: a head/tail join could cut an
    /// opaque credential into fragments that no format matcher can recognize.
    /// Returns stdout and stderr separately so a caller can find each stream's
    /// final lines; join them with [`combine_streams`] for the display body.
    pub(crate) fn read_complete_for_notice(&self) -> Option<(String, String)> {
        let read = |stream| {
            let source = self.source(stream)?;
            let read = read_source_bounded(source, DISK_LIMIT_BYTES as usize).ok()?;
            (!read.truncated).then_some(read.text)
        };
        Some((read(StreamKind::Stdout)?, read(StreamKind::Stderr)?))
    }

    pub fn stream_len(&self, stream: StreamKind) -> u64 {
        self.source(stream)
            .and_then(|source| source.sampled().ok().map(|(_, len)| len))
            .unwrap_or(0)
    }

    pub fn read_for_token_count(&self, max_bytes_per_stream: usize) -> TokenCountInput {
        match self {
            Self::Pipes { stdout, stderr } => {
                let stdout = read_source_tail(stdout, max_bytes_per_stream);
                let stderr = read_source_tail(stderr, max_bytes_per_stream);
                match (stdout, stderr) {
                    (Ok((stdout, _)), Ok((stderr, _))) => TokenCountInput::Text(combine_streams(
                        String::from_utf8_lossy(&stdout).as_ref(),
                        String::from_utf8_lossy(&stderr).as_ref(),
                    )),
                    (Ok((stdout, _)), Err(_)) => TokenCountInput::Text(combine_streams(
                        String::from_utf8_lossy(&stdout).as_ref(),
                        "",
                    )),
                    (Err(_), Ok((stderr, _))) => TokenCountInput::Text(combine_streams(
                        "",
                        String::from_utf8_lossy(&stderr).as_ref(),
                    )),
                    (Err(_), Err(_)) => TokenCountInput::Skipped,
                }
            }
            Self::Pty { .. } => TokenCountInput::Skipped,
        }
    }

    pub fn output_path(&self) -> Option<PathBuf> {
        match self {
            Self::Pipes { stdout, .. } => Some(stdout.path().to_path_buf()),
            Self::Pty { combined } => Some(combined.path().to_path_buf()),
        }
    }

    pub fn enforce_terminal_cap(&mut self) -> DiskTruncation {
        match self {
            Self::Pipes { stdout, stderr } => DiskTruncation {
                stdout_prefix_bytes: stdout.replace_with_tail(DISK_LIMIT_BYTES).unwrap_or(0),
                stderr_prefix_bytes: stderr.replace_with_tail(DISK_LIMIT_BYTES).unwrap_or(0),
                combined_prefix_bytes: 0,
            },
            Self::Pty { combined } => DiskTruncation {
                stdout_prefix_bytes: 0,
                stderr_prefix_bytes: 0,
                combined_prefix_bytes: combined.replace_with_tail(DISK_LIMIT_BYTES).unwrap_or(0),
            },
        }
    }

    fn source(&self, stream: StreamKind) -> Option<&ArtifactSource> {
        match (self, stream) {
            (Self::Pipes { stdout, .. }, StreamKind::Stdout) => Some(stdout),
            (Self::Pipes { stderr, .. }, StreamKind::Stderr) => Some(stderr),
            (Self::Pty { combined }, _) => Some(combined),
        }
    }
}

fn empty_bounded_read() -> BoundedRead {
    BoundedRead {
        text: String::new(),
        truncated: false,
        total_bytes: 0,
    }
}

#[cfg(test)]
fn tail_reads() -> &'static Mutex<std::collections::HashMap<PathBuf, usize>> {
    TAIL_READS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
fn tail_read_key(buffer: &BgBuffer) -> &Path {
    match buffer {
        BgBuffer::Pipes { stdout, .. } => stdout.path(),
        BgBuffer::Pty { combined } => combined.path(),
    }
}

#[cfg(test)]
fn bump_tail_read_count(buffer: &BgBuffer) {
    if let Ok(mut reads) = tail_reads().lock() {
        *reads
            .entry(tail_read_key(buffer).to_path_buf())
            .or_default() += 1;
    }
}

#[cfg(test)]
pub(crate) fn reset_tail_read_count(path: &Path) {
    if let Ok(mut reads) = tail_reads().lock() {
        reads.remove(path);
    }
}

#[cfg(test)]
pub(crate) fn tail_read_count(path: &Path) -> usize {
    tail_reads()
        .lock()
        .ok()
        .and_then(|reads| reads.get(path).copied())
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenCountInput {
    Text(String),
    Skipped,
}

pub fn combine_streams(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn read_source_tail(source: &ArtifactSource, max_bytes: usize) -> io::Result<(Vec<u8>, bool)> {
    let (mut bytes, truncated) = retry_artifact_read(|| {
        let mut file = source.open_once()?;
        if max_bytes == 0 {
            return Ok((Vec::new(), source.len(&file)? > 0));
        }
        source.record_result("read_tail", file.tail(max_bytes), |(bytes, _)| {
            Some(bytes.len() as u64)
        })
    })?;
    if truncated {
        bytes = align_start_to_utf8(bytes);
    }
    Ok((bytes, truncated))
}

#[cfg(test)]
pub(crate) fn read_file_tail(path: &Path, max_bytes: usize) -> io::Result<(Vec<u8>, bool)> {
    read_source_tail(&ArtifactSource::Exact(path.to_path_buf()), max_bytes)
}

fn read_source_bounded(source: &ArtifactSource, max_bytes: usize) -> io::Result<BoundedRead> {
    let (mut file, total_bytes) = source.sampled()?;
    if total_bytes > max_bytes as u64 {
        if max_bytes == 0 {
            return Ok(BoundedRead {
                text: String::new(),
                truncated: true,
                total_bytes,
            });
        }
        return read_single_file_head_tail(
            source,
            max_bytes,
            max_bytes / 2,
            max_bytes - max_bytes / 2,
        );
    }
    let bytes = source.read_range(&mut file, 0, total_bytes)?;
    Ok(BoundedRead {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated: false,
        total_bytes,
    })
}

#[cfg(test)]
fn read_file_bounded(path: &Path, max_bytes: usize) -> io::Result<BoundedRead> {
    read_source_bounded(&ArtifactSource::Exact(path.to_path_buf()), max_bytes)
}

fn read_single_file_head_tail(
    source: &ArtifactSource,
    max_bytes: usize,
    head_bytes: usize,
    tail_bytes: usize,
) -> io::Result<BoundedRead> {
    let (mut file, total_bytes) = source.sampled()?;
    if total_bytes <= max_bytes as u64 {
        let bytes = source.read_range(&mut file, 0, total_bytes)?;
        return Ok(BoundedRead {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated: false,
            total_bytes,
        });
    }
    let head_len = head_bytes.min(max_bytes) as u64;
    let tail_len = tail_bytes.min(max_bytes.saturating_sub(head_len as usize)) as u64;
    let head = read_source_range(source, 0, head_len)?;
    let tail_start = total_bytes.saturating_sub(tail_len);
    let tail = read_source_range(source, tail_start, tail_len)?;
    Ok(BoundedRead {
        text: join_head_tail_bytes(head, tail, total_bytes.saturating_sub(head_len + tail_len)),
        truncated: true,
        total_bytes,
    })
}

fn read_two_file_head_tail(
    first: &ArtifactSource,
    second: &ArtifactSource,
    max_bytes: usize,
    head_bytes: usize,
    tail_bytes: usize,
) -> (BoundedRead, usize) {
    let (mut first_file, first_len) = first
        .sampled()
        .map(|(file, len)| (Some(file), len))
        .unwrap_or((None, 0));
    let (mut second_file, second_len) = second
        .sampled()
        .map(|(file, len)| (Some(file), len))
        .unwrap_or((None, 0));
    let total_bytes = first_len.saturating_add(second_len);
    if total_bytes <= max_bytes as u64 {
        let first_bytes = first_file
            .as_mut()
            .map(|file| first.read_range(file, 0, first_len))
            .transpose()
            .map(Option::unwrap_or_default)
            .unwrap_or_default();
        let second_bytes = second_file
            .as_mut()
            .map(|file| second.read_range(file, 0, second_len))
            .transpose()
            .map(Option::unwrap_or_default)
            .unwrap_or_default();
        let first_text = String::from_utf8_lossy(&first_bytes);
        let boundary = first_text.len();
        return (
            BoundedRead {
                text: format!("{first_text}{}", String::from_utf8_lossy(&second_bytes)),
                truncated: false,
                total_bytes,
            },
            boundary,
        );
    }
    let head_budget = head_bytes.min(max_bytes);
    let (first_head, second_head) = split_stream_budget(first_len, second_len, head_budget);
    let tail_budget = tail_bytes.min(max_bytes.saturating_sub(first_head + second_head));
    let first_remaining = first_len.saturating_sub(first_head as u64);
    let second_remaining = second_len.saturating_sub(second_head as u64);
    let (first_tail, second_tail) =
        split_stream_budget(first_remaining, second_remaining, tail_budget);
    let first_read =
        read_single_file_head_tail(first, first_head + first_tail, first_head, first_tail)
            .unwrap_or_else(|_| empty_bounded_read());
    let second_read =
        read_single_file_head_tail(second, second_head + second_tail, second_head, second_tail)
            .unwrap_or_else(|_| empty_bounded_read());
    let boundary = first_read.text.len();
    (
        BoundedRead {
            text: combine_streams(&first_read.text, &second_read.text),
            truncated: true,
            total_bytes,
        },
        boundary,
    )
}

fn read_two_file_tails(
    first: &ArtifactSource,
    second: &ArtifactSource,
    max_bytes: usize,
) -> (String, bool) {
    let (mut first_file, first_len) = first
        .sampled()
        .map(|(file, len)| (Some(file), len))
        .unwrap_or((None, 0));
    let (mut second_file, second_len) = second
        .sampled()
        .map(|(file, len)| (Some(file), len))
        .unwrap_or((None, 0));
    let total_bytes = first_len.saturating_add(second_len);
    if total_bytes <= max_bytes as u64 {
        let first_bytes = first_file
            .as_mut()
            .map(|file| first.read_range(file, 0, first_len))
            .transpose()
            .map(Option::unwrap_or_default)
            .unwrap_or_default();
        let second_bytes = second_file
            .as_mut()
            .map(|file| second.read_range(file, 0, second_len))
            .transpose()
            .map(Option::unwrap_or_default)
            .unwrap_or_default();
        return (
            combine_streams(
                String::from_utf8_lossy(&first_bytes).as_ref(),
                String::from_utf8_lossy(&second_bytes).as_ref(),
            ),
            false,
        );
    }
    let (first_budget, second_budget) = split_stream_budget(first_len, second_len, max_bytes);
    let (first_bytes, first_truncated) = read_source_tail(first, first_budget)
        .unwrap_or_else(|_| (Vec::new(), first_len > first_budget as u64));
    let (second_bytes, second_truncated) = read_source_tail(second, second_budget)
        .unwrap_or_else(|_| (Vec::new(), second_len > second_budget as u64));
    (
        combine_streams(
            String::from_utf8_lossy(&first_bytes).as_ref(),
            String::from_utf8_lossy(&second_bytes).as_ref(),
        ),
        first_truncated || second_truncated || total_bytes > max_bytes as u64,
    )
}

fn split_stream_budget(first_len: u64, second_len: u64, total_budget: usize) -> (usize, usize) {
    if total_budget == 0 {
        return (0, 0);
    }
    match (first_len > 0, second_len > 0) {
        (false, false) => (0, 0),
        (true, false) => (total_budget, 0),
        (false, true) => (0, total_budget),
        (true, true) => {
            let mut first_budget = total_budget / 2;
            let mut second_budget = total_budget - first_budget;
            redistribute_unused_budget(first_len, &mut first_budget, &mut second_budget);
            redistribute_unused_budget(second_len, &mut second_budget, &mut first_budget);
            (first_budget, second_budget)
        }
    }
}

fn redistribute_unused_budget(len: u64, own_budget: &mut usize, other_budget: &mut usize) {
    let needed = len.min(usize::MAX as u64) as usize;
    if needed < *own_budget {
        let spare = own_budget.saturating_sub(needed);
        *own_budget = needed;
        *other_budget = other_budget.saturating_add(spare);
    }
}

fn read_source_range(source: &ArtifactSource, start: u64, len: u64) -> io::Result<Vec<u8>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut file = source.open()?;
    let mut bytes = source.read_range(&mut file, start, len)?;
    if start > 0 {
        bytes = align_start_to_utf8(bytes);
    }
    Ok(align_end_to_utf8(bytes))
}

#[cfg(test)]
fn read_file_range(path: &Path, start: u64, len: u64) -> io::Result<Vec<u8>> {
    read_source_range(&ArtifactSource::Exact(path.to_path_buf()), start, len)
}

fn join_head_tail_bytes(head: Vec<u8>, tail: Vec<u8>, truncated_bytes: u64) -> String {
    let mut output = String::from_utf8_lossy(&head).into_owned();
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str("...<truncated ");
    output.push_str(&truncated_bytes.to_string());
    output.push_str(" bytes>...\n");
    output.push_str(&String::from_utf8_lossy(&tail));
    output
}

#[cfg(test)]
fn truncate_front(path: &Path, retain_bytes: u64) -> io::Result<u64> {
    super::persistence::replace_unregistered_with_tail(path, retain_bytes)
}

fn align_start_to_utf8(mut bytes: Vec<u8>) -> Vec<u8> {
    let mut start = 0;
    while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
        start += 1;
    }
    if start > 0 {
        bytes.drain(..start);
    }
    bytes
}

fn align_end_to_utf8(bytes: Vec<u8>) -> Vec<u8> {
    align_end_to_utf8_with(bytes, || {})
}

fn align_end_to_utf8_with(mut bytes: Vec<u8>, mut visit: impl FnMut()) -> Vec<u8> {
    while !bytes.is_empty() {
        visit();
        let last = bytes.len() - 1;
        if bytes[last] < 0x80 {
            break;
        }
        let lead_pos = if (bytes[last] & 0xC0) == 0x80 {
            let mut pos = last;
            while pos > 0 && (bytes[pos] & 0xC0) == 0x80 {
                visit();
                pos -= 1;
            }
            if (bytes[pos] & 0xC0) == 0xC0 {
                pos
            } else {
                // No lead byte exists for this continuation run. Discard the
                // whole run rather than rescanning it after each popped byte.
                bytes.truncate(if bytes[pos] < 0x80 { pos + 1 } else { pos });
                continue;
            }
        } else {
            last
        };
        let lead = bytes[lead_pos];
        debug_assert!(lead >= 0xC0, "lead byte must be >= 0xC0, got {lead:#x}");
        let expected = if lead < 0xE0 {
            1
        } else if lead < 0xF0 {
            2
        } else {
            3
        };
        if last - lead_pos >= expected {
            break;
        }
        bytes.truncate(lead_pos);
    }
    bytes
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_bounded_reads_ignore_growth_after_stat() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("stdout");
        let second = dir.path().join("stderr");
        std::fs::write(&first, "abcd").unwrap();
        std::fs::write(&second, "efgh").unwrap();
        let source = ArtifactSource::Exact(first.clone());
        let other = ArtifactSource::Exact(second);
        for operation in 0..4 {
            std::fs::write(&first, "abcd").unwrap();
            GROW_AFTER_STAT.with(|hook| *hook.borrow_mut() = Some(vec![b'x'; 1024]));
            let text = match operation {
                0 => read_source_bounded(&source, 8).unwrap().text,
                1 => read_single_file_head_tail(&source, 8, 4, 4).unwrap().text,
                2 => read_two_file_head_tail(&source, &other, 8, 4, 4).0.text,
                _ => read_two_file_tails(&source, &other, 8).0,
            };
            assert!(
                !text.contains('x'),
                "operation {operation} read past its sampled length"
            );
        }
    }

    #[test]
    fn lifecycle_utf8_alignment_visits_invalid_suffix_once() {
        for prefix in [vec![], b"ascii".to_vec(), vec![0xff, 0xfe]] {
            let mut input = prefix.clone();
            input.extend(std::iter::repeat_n(0x80, 1024));
            let mut visits = 0;
            let output = align_end_to_utf8_with(input.clone(), || visits += 1);
            assert!(
                visits <= input.len() * 2,
                "invalid suffix was rescanned {visits} times"
            );
            let expected = if prefix.is_empty() {
                vec![]
            } else if prefix[0] == b'a' {
                prefix
            } else {
                input
            };
            assert_eq!(output, expected);
        }
    }

    // --- Regression tests for UTF-8 splitting at byte boundaries ---
    // CORRECT behavior: read_file_tail should not split UTF-8 characters.
    // These tests FAIL when the bug is present.

    #[test]
    fn read_file_tail_should_not_split_utf8_character() {
        // "AAAA€" = 7 bytes (4 ASCII + 3-byte €).
        // 2-byte tail reads bytes [5,6] = 0x82 0xAC - incomplete trailing
        // bytes of €. from_utf8_lossy produces U+FFFD.
        // CORRECT: no replacement character should appear.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(&path, "AAAA€".as_bytes()).unwrap();
        let (bytes, _truncated) = read_file_tail(&path, 2).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains('\u{FFFD}'),
            "read_file_tail should not produce replacement characters, got: {:?}",
            text
        );
    }

    #[test]
    fn truncate_front_should_not_split_utf8_character() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(&path, "AAAA€".as_bytes()).unwrap();
        truncate_front(&path, 2).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains('\u{FFFD}'),
            "truncate_front should not produce replacement characters, got: {:?}",
            text
        );
    }

    #[test]
    fn read_file_tail_should_not_split_4byte_utf8() {
        // "AAAA😀" = 4 + 4 = 8 bytes. 2-byte tail reads bytes [6,7] = incomplete.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(&path, "AAAA😀".as_bytes()).unwrap();
        let (bytes, _truncated) = read_file_tail(&path, 2).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains('\u{FFFD}'),
            "read_file_tail should not produce replacement characters for 4-byte chars, got: {:?}",
            text
        );
    }

    #[test]
    fn read_file_range_end_boundary_should_not_split_utf8() {
        // "AAAA€" = 7 bytes. read_file_range(path, 0, 5) reads bytes [0..5].
        // byte 4 = 0xE2 (lead of €), byte 5 = 0x82 (continuation) — not included.
        // End at byte 5 splits after the lead byte. align_end_to_utf8 should trim it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(&path, "AAAA€".as_bytes()).unwrap();
        let bytes = read_file_range(&path, 0, 5).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains('\u{FFFD}'),
            "read_file_range should not produce replacement characters at end boundary, got: {:?}",
            text
        );
    }

    #[test]
    fn ascii_content_unaffected_by_alignment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        let content = b"hello world\nline two\n";
        std::fs::write(&path, content).unwrap();
        let (bytes, truncated) = read_file_tail(&path, 10).unwrap();
        assert!(truncated);
        assert_eq!(bytes, b"\nline two\n");
    }

    #[test]
    fn read_file_range_start_boundary_should_not_split_utf8() {
        // "Hello€World" = 5 + 3 + 5 = 13 bytes.
        // read_file_range(path, 5, 4) reads bytes [5..9]:
        // bytes 5-7 = € (0xE2 0x82 0xAC), byte 8 = 'W'.
        // Start at byte 5 = 0xE2 (lead byte) — aligned, no split.
        // End at byte 9 = 'o' — aligned, no split.
        // But read_file_range(path, 6, 2) reads bytes [6..8]:
        // byte 6 = 0x82 (continuation), byte 7 = 0xAC (continuation).
        // Start at byte 6 splits inside €. align_start_to_utf8 should skip.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(&path, b"Hello\xe2\x82\xacWorld").unwrap();
        let bytes = read_file_range(&path, 6, 2).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains('\u{FFFD}'),
            "read_file_range with start>0 should not produce replacement characters, got: {:?}",
            text
        );
    }

    // --- Regression test for stdout/stderr interleaving ---
    // This test documents the limitation: stdout always comes before stderr
    // in the combined output, regardless of temporal write order.
    // It does not assert correct interleaving (that would require a redesign)
    // but verifies the current behavior is what we expect.

    #[test]
    fn read_tail_puts_stdout_before_stderr() {
        // Write stdout and stderr to separate files, then verify
        // the combined output has stdout content before stderr content.
        let dir = tempfile::tempdir().unwrap();
        let stdout_path = dir.path().join("stdout");
        let stderr_path = dir.path().join("stderr");
        std::fs::write(&stdout_path, b"stdout-line\n").unwrap();
        std::fs::write(&stderr_path, b"stderr-line\n").unwrap();
        let buffer = BgBuffer::new(stdout_path, stderr_path);
        let (text, _) = buffer.read_tail(1024);
        let stdout_pos = text.find("stdout-line").unwrap();
        let stderr_pos = text.find("stderr-line").unwrap();
        assert!(
            stdout_pos < stderr_pos,
            "stdout should come before stderr in combined output"
        );
    }

    #[test]
    fn read_tail_preserves_each_stream_tail_when_combined_cap_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let stdout_path = dir.path().join("stdout");
        let stderr_path = dir.path().join("stderr");
        std::fs::write(
            &stdout_path,
            format!(
                "{}
error: stdout boom
",
                "stdout noise
"
                .repeat(20)
            ),
        )
        .unwrap();
        std::fs::write(
            &stderr_path,
            format!(
                "{}
stderr tail
",
                "stderr noise
"
                .repeat(200)
            ),
        )
        .unwrap();
        let buffer = BgBuffer::new(stdout_path, stderr_path);

        let (text, truncated) = buffer.read_tail(160);

        assert!(truncated);
        assert!(text.contains("error: stdout boom"));
        assert!(text.contains("stderr tail"));
    }

    #[test]
    fn read_combined_head_tail_preserves_each_stream_tail() {
        let dir = tempfile::tempdir().unwrap();
        let stdout_path = dir.path().join("stdout");
        let stderr_path = dir.path().join("stderr");
        std::fs::write(
            &stdout_path,
            format!(
                "stdout head
{}
ERROR: stdout final
",
                "x".repeat(512)
            ),
        )
        .unwrap();
        std::fs::write(
            &stderr_path,
            format!(
                "stderr head
{}
stderr final
",
                "y".repeat(2048)
            ),
        )
        .unwrap();
        let buffer = BgBuffer::new(stdout_path, stderr_path);

        let read = buffer.read_combined_head_tail(256, 64, 192);

        assert!(read.truncated);
        assert!(read.text.contains("ERROR: stdout final"));
        assert!(read.text.contains("stderr final"));
        let (split_read, boundary) = buffer.read_combined_head_tail_with_boundary(256, 64, 192);
        assert_eq!(split_read, read);
        assert!(split_read.text[..boundary].ends_with("ERROR: stdout final\n"));
        assert!(split_read.text[boundary..].ends_with("stderr final\n"));
    }

    #[test]
    fn read_file_bounded_returns_head_and_tail_for_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(
            &path,
            format!(
                "HEAD
{}
TAIL",
                "x".repeat(256)
            ),
        )
        .unwrap();

        let read = read_file_bounded(&path, 64).unwrap();

        assert!(read.truncated);
        assert!(read.text.contains("HEAD"));
        assert!(read.text.contains("TAIL"));
        assert!(read.text.contains("...<truncated "));
    }

    #[test]
    fn truncate_front_reports_prefix_bytes_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(
            &path,
            b"early root cause
late tail
",
        )
        .unwrap();

        let removed = truncate_front(&path, 10).unwrap();
        let retained = std::fs::read_to_string(&path).unwrap();

        assert!(removed > 0);
        assert!(!retained.contains("early root cause"));
        assert!(retained.contains("late tail"));
    }
    #[test]
    fn recovered_read_diagnostics_preserve_output_and_report_missing_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let stdout = dir.path().join("stdout");
        let stderr = dir.path().join("missing-stderr");
        std::fs::write(&stdout, b"recovered-output").unwrap();
        let buffer = BgBuffer::new(stdout.clone(), stderr.clone());
        with_recovered_read_diagnostics("diagnostic-task", || {
            let read = buffer.read_combined_head_tail(1024, 512, 512);
            assert_eq!(read.text, "recovered-output");
            let messages = RECOVERED_READ_DIAGNOSTICS
                .with(|slot| slot.borrow().as_ref().unwrap().messages.clone());
            assert!(
                messages.iter().any(
                    |message| message.contains("operation=read_range result=ok bytes=Some(16)")
                ),
                "successful stdout read was not logged: {messages:?}"
            );
            assert!(
                messages
                    .iter()
                    .any(|message| message.contains(&stderr.display().to_string())
                        && message.contains("operation=open result=error kind=NotFound")),
                "missing stderr read was not logged: {messages:?}"
            );
        });
        assert!(
            RECOVERED_READ_DIAGNOSTICS.with(|slot| slot.borrow().is_none()),
            "recovered-read scope leaked to the next render"
        );
    }
}
