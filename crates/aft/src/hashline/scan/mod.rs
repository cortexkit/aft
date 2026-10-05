//! Coherent byte-level scanning for hashline snapshots.
//!
//! A hashline snapshot is built from one forward pass over one byte stream.  The
//! pass has two deliberately separate products: the complete input is used to
//! derive the normalized whole-file tag, while only records selected by the
//! [`CoverageInput`] are retained and therefore become seen.  This distinction
//! is important for ranged reads: walking past a range to finish the hash and
//! observe EOF must not silently authorize edits to the rows that were walked
//! past.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use crate::hashline::oracle::{normalize_for_tag, tag_for};

/// The maximum size of a read for which callers may choose to publish a
/// taggable snapshot.  The scanner itself does not enforce this policy: the
/// read layer owns the user-facing oversize refusal, while this module remains
/// useful for byte-model and streaming tests.
pub const MAX_FILE_READ_BYTES: u64 = 64 * 1024 * 1024;

/// The line terminator retained as part of a raw line record's exact identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TerminatorKind {
    /// A single LF byte terminated the record.
    Lf,
    /// A CR byte immediately followed by LF and both bytes terminated the
    /// record.
    CrLf,
    /// The record ended at EOF without a terminator.
    None,
}

impl TerminatorKind {
    /// Uppercase aliases make byte-model fixtures read naturally without
    /// changing Rust's conventional enum variant spelling.
    pub const LF: Self = Self::Lf;
    pub const CRLF: Self = Self::CrLf;
}

pub type Terminator = TerminatorKind;

impl fmt::Display for TerminatorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
            Self::None => "none",
        })
    }
}

/// Content bytes and terminator bytes for one line.
///
/// `content` never contains the line terminator, but it does contain every
/// other byte, including a UTF-8 BOM on line one and carriage returns that are
/// not immediately followed by LF.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawLineRecord {
    pub content: Vec<u8>,
    pub terminator: TerminatorKind,
}

pub type RawLine = RawLineRecord;

impl RawLineRecord {
    pub fn new(content: Vec<u8>, terminator: TerminatorKind) -> Self {
        Self {
            content,
            terminator,
        }
    }

    pub fn content(&self) -> &[u8] {
        &self.content
    }

    pub fn terminator(&self) -> TerminatorKind {
        self.terminator
    }

    /// Return the exact bytes represented by this record.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = self.content.clone();
        match self.terminator {
            TerminatorKind::Lf => bytes.push(b'\n'),
            TerminatorKind::CrLf => bytes.extend_from_slice(b"\r\n"),
            TerminatorKind::None => {}
        }
        bytes
    }
}

/// A retained record with its absolute line number and byte span.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedLine {
    pub line_number: usize,
    pub record: RawLineRecord,
    /// Inclusive start offset in the original byte stream.
    pub byte_start: u64,
    /// Exclusive end offset in the original byte stream, including the
    /// terminator when one was present.
    pub byte_end: u64,
}

impl RetainedLine {
    pub fn content(&self) -> &[u8] {
        self.record.content()
    }

    pub fn terminator(&self) -> TerminatorKind {
        self.record.terminator()
    }
}

/// The rows a scan is allowed to retain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverageInput {
    /// Requested absolute 1-based rows.  Rows outside the file remain
    /// requested but are not seen or eligible.
    pub requested_lines: BTreeSet<usize>,
    /// Retain every complete record encountered by the scan.
    pub retain_all: bool,
}

impl Default for CoverageInput {
    fn default() -> Self {
        Self::none()
    }
}

impl CoverageInput {
    pub fn none() -> Self {
        Self {
            requested_lines: BTreeSet::new(),
            retain_all: false,
        }
    }

    pub fn whole_file() -> Self {
        Self {
            requested_lines: BTreeSet::new(),
            retain_all: true,
        }
    }

    pub fn line(line_number: usize) -> Self {
        Self::lines([line_number])
    }

    pub fn lines<I>(lines: I) -> Self
    where
        I: IntoIterator<Item = usize>,
    {
        Self {
            requested_lines: lines.into_iter().collect(),
            retain_all: false,
        }
    }

    pub fn range(first: usize, last: usize) -> Self {
        if first > last {
            return Self::none();
        }
        Self {
            requested_lines: (first..=last).collect(),
            retain_all: false,
        }
    }

    pub fn retains(&self, line_number: usize) -> bool {
        self.retain_all || self.requested_lines.contains(&line_number)
    }

    pub fn is_whole_file(&self) -> bool {
        self.retain_all
    }
}

/// Caller-supplied context carried through a scan and into the published
/// snapshot.  It is diagnostic provenance, not verification evidence.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CaptureProvenance {
    pub source: Option<PathBuf>,
    pub file_identity: Option<FileIdentity>,
    pub byte_len: Option<u64>,
    pub modified: Option<SystemTime>,
    /// Extra capture labels are useful to transports and deterministic tests;
    /// they are intentionally ignored by snapshot equivalence.
    pub labels: BTreeMap<String, String>,
}

impl CaptureProvenance {
    pub fn from_source(source: impl Into<PathBuf>) -> Self {
        Self {
            source: Some(source.into()),
            ..Self::default()
        }
    }

    pub fn with_source(mut self, source: impl Into<PathBuf>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn with_byte_len(mut self, byte_len: u64) -> Self {
        self.byte_len = Some(byte_len);
        self
    }

    pub fn with_file_identity(mut self, file_identity: FileIdentity) -> Self {
        self.file_identity = Some(file_identity);
        self
    }

    pub fn with_modified(mut self, modified: SystemTime) -> Self {
        self.modified = Some(modified);
        self
    }

    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// Build metadata observed from an already-open descriptor.  Keeping this
    /// operation descriptor-based prevents the capture from accidentally
    /// switching to a different path entry between the scan and its checks.
    pub fn from_metadata(source: impl Into<PathBuf>, metadata: &Metadata) -> Self {
        Self {
            source: Some(source.into()),
            file_identity: file_identity(metadata),
            byte_len: Some(metadata.len()),
            modified: metadata.modified().ok(),
            labels: BTreeMap::new(),
        }
    }

    /// Compare only metadata that both observations provide.  Source labels
    /// and arbitrary labels are not version evidence.
    pub fn same_file_version(&self, other: &Self) -> bool {
        if self.file_identity.is_some()
            && other.file_identity.is_some()
            && self.file_identity != other.file_identity
        {
            return false;
        }
        if self.byte_len.is_some() && other.byte_len.is_some() && self.byte_len != other.byte_len {
            return false;
        }
        if self.modified.is_some() && other.modified.is_some() && self.modified != other.modified {
            return false;
        }
        true
    }
}

/// Stable identity fields available on platforms where standard filesystem
/// metadata exposes them.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

#[cfg(unix)]
fn file_identity(metadata: &Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;

    Some(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn file_identity(_metadata: &Metadata) -> Option<FileIdentity> {
    None
}

/// The caller's requested coverage and provenance inputs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanRequest {
    pub coverage: CoverageInput,
    pub provenance: CaptureProvenance,
}

impl Default for ScanRequest {
    fn default() -> Self {
        Self::whole_file()
    }
}

impl ScanRequest {
    pub fn new(coverage: CoverageInput) -> Self {
        Self {
            coverage,
            provenance: CaptureProvenance::default(),
        }
    }

    pub fn whole_file() -> Self {
        Self::new(CoverageInput::whole_file())
    }

    pub fn for_lines<I>(lines: I) -> Self
    where
        I: IntoIterator<Item = usize>,
    {
        Self::new(CoverageInput::lines(lines))
    }

    pub fn for_range(first: usize, last: usize) -> Self {
        Self::new(CoverageInput::range(first, last))
    }

    pub fn with_provenance(mut self, provenance: CaptureProvenance) -> Self {
        self.provenance = provenance;
        self
    }
}

/// Facts produced by a scan.  A row is seen iff its complete raw record was
/// retained; `scanned_line_count` alone never grants eligibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanCoverage {
    pub requested_lines: BTreeSet<usize>,
    pub retain_all: bool,
    pub retained_lines: BTreeSet<usize>,
    pub seen_lines: BTreeSet<usize>,
    pub scanned_line_count: usize,
    pub total_lines: usize,
    pub byte_count: u64,
    pub eof_observed: bool,
}

pub type Coverage = ScanCoverage;

impl ScanCoverage {
    pub fn is_seen(&self, line_number: usize) -> bool {
        self.seen_lines.contains(&line_number)
    }

    pub fn is_retained(&self, line_number: usize) -> bool {
        self.retained_lines.contains(&line_number)
    }

    pub fn is_eligible(&self, line_number: usize) -> bool {
        self.is_seen(line_number)
    }

    pub fn scanned_to_eof(&self) -> bool {
        self.eof_observed
    }
}

/// Boundary facts that can be used by later address resolution without
/// consulting the current live-file length.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundaryEvidence {
    pub empty_file: bool,
    pub bof_observed: bool,
    pub eof_observed: bool,
    pub first_seen: Option<usize>,
    pub last_seen: Option<usize>,
}

/// A published, coherent snapshot of one whole-file scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub tag: String,
    pub normalized_bytes: Vec<u8>,
    pub records: BTreeMap<usize, RawLineRecord>,
    pub retained_lines: BTreeMap<usize, RetainedLine>,
    pub coverage: ScanCoverage,
    pub boundary: BoundaryEvidence,
    pub total_lines: usize,
    pub byte_count: u64,
    /// Caller-provided context; it is not used to authorize a write.
    pub provenance: CaptureProvenance,
    /// Metadata observed on the descriptor used for this scan.
    pub capture_provenance: CaptureProvenance,
}

impl Snapshot {
    pub fn raw_record(&self, line_number: usize) -> Option<&RawLineRecord> {
        self.records.get(&line_number)
    }

    pub fn retained_line(&self, line_number: usize) -> Option<&RetainedLine> {
        self.retained_lines.get(&line_number)
    }

    pub fn is_seen(&self, line_number: usize) -> bool {
        self.coverage.is_seen(line_number)
    }

    pub fn eof_observed(&self) -> bool {
        self.coverage.eof_observed
    }

    pub fn verify_record(&self, line_number: usize, expected: &RawLineRecord) -> bool {
        self.raw_record(line_number) == Some(expected)
    }
}

/// Result of a completed forward scan.  `snapshot` is `Some` only after EOF
/// was observed; no partial result can be published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanResult {
    pub snapshot: Option<Snapshot>,
    pub coverage: ScanCoverage,
    pub provenance: CaptureProvenance,
    pub capture_provenance: CaptureProvenance,
}

impl ScanResult {
    pub fn published_snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    pub fn into_snapshot(self) -> Option<Snapshot> {
        self.snapshot
    }
}

/// Errors from the byte scanner itself.
#[derive(Debug)]
pub enum ScanError {
    Io(io::Error),
    AlreadyFinished,
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "hashline scan I/O error: {error}"),
            Self::AlreadyFinished => formatter.write_str("hashline scan already reached EOF"),
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::AlreadyFinished => None,
        }
    }
}

impl From<io::Error> for ScanError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// An incremental, forward-only scanner.
///
/// Feeding chunks does not publish a tag or snapshot.  Calling [`finish`]
/// observes EOF and performs the sole publication step, which makes the EOF
/// invariant explicit even for callers that stream from a descriptor.
pub struct ForwardScanner {
    request: ScanRequest,
    capture_provenance: CaptureProvenance,
    raw_bytes: Vec<u8>,
    current_line: Vec<u8>,
    current_line_start: u64,
    next_line_number: usize,
    scanned_line_count: usize,
    retained_lines: BTreeMap<usize, RetainedLine>,
    finished: bool,
    published: Option<Snapshot>,
}

impl ForwardScanner {
    pub fn new(request: ScanRequest) -> Self {
        let capture_provenance = CaptureProvenance::default();
        Self {
            request,
            capture_provenance,
            raw_bytes: Vec::new(),
            current_line: Vec::new(),
            current_line_start: 0,
            next_line_number: 1,
            scanned_line_count: 0,
            retained_lines: BTreeMap::new(),
            finished: false,
            published: None,
        }
    }

    pub fn with_capture_provenance(
        request: ScanRequest,
        capture_provenance: CaptureProvenance,
    ) -> Self {
        let mut scanner = Self::new(request);
        scanner.capture_provenance = capture_provenance;
        scanner
    }

    /// Feed one forward chunk.  A zero-length chunk is harmless and is not
    /// treated as EOF; the reader wrapper calls [`finish`] after its read loop.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ScanError> {
        if self.finished {
            return Err(ScanError::AlreadyFinished);
        }

        let chunk_start = self.raw_bytes.len() as u64;
        self.raw_bytes.extend_from_slice(bytes);
        let mut consumed = 0u64;
        for segment in bytes.split_inclusive(|byte| *byte == b'\n') {
            consumed += segment.len() as u64;
            if segment.ends_with(b"\n") {
                self.current_line
                    .extend_from_slice(&segment[..segment.len() - 1]);
                let mut content = std::mem::take(&mut self.current_line);
                let terminator = if content.last() == Some(&b'\r') {
                    content.pop();
                    TerminatorKind::CrLf
                } else {
                    TerminatorKind::Lf
                };
                self.scanned_line_count += 1;
                let line_number = self.next_line_number;
                self.next_line_number += 1;
                let byte_end = chunk_start + consumed;
                self.retain_line_if_requested(
                    line_number,
                    RawLineRecord::new(content, terminator),
                    self.current_line_start,
                    byte_end,
                );
                self.current_line_start = byte_end;
            } else {
                self.current_line.extend_from_slice(segment);
            }
        }
        Ok(())
    }

    /// No snapshot is available until this method is called successfully.
    pub fn published_snapshot(&self) -> Option<&Snapshot> {
        self.published.as_ref()
    }

    pub fn eof_observed(&self) -> bool {
        self.finished
    }

    /// Finish the one forward scan and publish its complete snapshot.
    pub fn finish(&mut self) -> Result<ScanResult, ScanError> {
        if self.finished {
            return Err(ScanError::AlreadyFinished);
        }

        if !self.current_line.is_empty() {
            self.scanned_line_count += 1;
            let line_number = self.next_line_number;
            let byte_end = self.raw_bytes.len() as u64;
            let content = std::mem::take(&mut self.current_line);
            self.retain_line_if_requested(
                line_number,
                RawLineRecord::new(content, TerminatorKind::None),
                self.current_line_start,
                byte_end,
            );
        }

        self.finished = true;
        let requested_lines = self.request.coverage.requested_lines.clone();
        let retained_line_numbers: BTreeSet<usize> = self.retained_lines.keys().copied().collect();
        let coverage = ScanCoverage {
            requested_lines,
            retain_all: self.request.coverage.retain_all,
            retained_lines: retained_line_numbers.clone(),
            seen_lines: retained_line_numbers,
            scanned_line_count: self.scanned_line_count,
            total_lines: self.scanned_line_count,
            byte_count: self.raw_bytes.len() as u64,
            eof_observed: true,
        };
        let first_seen = self.retained_lines.keys().next().copied();
        let last_seen = self.retained_lines.keys().next_back().copied();
        let boundary = BoundaryEvidence {
            empty_file: self.raw_bytes.is_empty(),
            bof_observed: true,
            eof_observed: true,
            first_seen,
            last_seen,
        };
        let records = self
            .retained_lines
            .iter()
            .map(|(&line_number, retained)| (line_number, retained.record.clone()))
            .collect();
        let normalized_bytes = normalize_for_tag(&self.raw_bytes);
        let tag = format!(
            "{:04X}",
            crate::hashline::oracle::xxhash32_seed_zero(&normalized_bytes) & 0xFFFF
        );
        let mut capture_provenance = self.capture_provenance.clone();
        capture_provenance.byte_len = Some(self.raw_bytes.len() as u64);
        let snapshot = Snapshot {
            tag,
            normalized_bytes,
            records,
            retained_lines: self.retained_lines.clone(),
            coverage: coverage.clone(),
            boundary,
            total_lines: self.scanned_line_count,
            byte_count: self.raw_bytes.len() as u64,
            provenance: self.request.provenance.clone(),
            capture_provenance: capture_provenance.clone(),
        };
        self.published = Some(snapshot.clone());
        Ok(ScanResult {
            snapshot: Some(snapshot),
            coverage,
            provenance: self.request.provenance.clone(),
            capture_provenance,
        })
    }

    fn retain_line_if_requested(
        &mut self,
        line_number: usize,
        record: RawLineRecord,
        byte_start: u64,
        byte_end: u64,
    ) {
        if self.request.coverage.retains(line_number) {
            self.retained_lines.insert(
                line_number,
                RetainedLine {
                    line_number,
                    record,
                    byte_start,
                    byte_end,
                },
            );
        }
    }
}

#[cfg(test)]
thread_local! {
    static SCANNER_BYTE_PUSHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Scan a reader once, to EOF, while retaining only the requested coverage.
pub fn scan_reader<R: Read>(reader: &mut R, request: ScanRequest) -> Result<ScanResult, ScanError> {
    scan_reader_with_provenance(reader, request, CaptureProvenance::default())
}

/// Scan a reader while attaching observed provenance supplied by the caller.
/// The caller can use this entry point for descriptor wrappers and deterministic
/// capture-instability tests; it still publishes only after [`ForwardScanner::finish`].
pub fn scan_reader_with_provenance<R: Read>(
    reader: &mut R,
    request: ScanRequest,
    capture_provenance: CaptureProvenance,
) -> Result<ScanResult, ScanError> {
    let mut scanner = ForwardScanner::with_capture_provenance(request, capture_provenance);
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        scanner.push(&buffer[..read])?;
    }
    scanner.finish()
}

/// Convenience whole-file scan for in-memory bytes.
pub fn scan_bytes(bytes: &[u8]) -> Snapshot {
    scan_bytes_with_request(bytes, ScanRequest::whole_file())
        .snapshot
        .expect("a byte slice scan always observes EOF")
}

/// In-memory scan with explicit coverage and provenance.
pub fn scan_bytes_with_request(bytes: &[u8], request: ScanRequest) -> ScanResult {
    let capture_provenance = request.provenance.clone().with_byte_len(bytes.len() as u64);
    let mut scanner = ForwardScanner::with_capture_provenance(request, capture_provenance);
    scanner
        .push(bytes)
        .expect("a new scanner cannot be finished");
    scanner
        .finish()
        .expect("a byte slice scan can finish exactly once")
}

pub fn scan_bytes_with_coverage(bytes: &[u8], coverage: CoverageInput) -> Snapshot {
    scan_bytes_with_request(bytes, ScanRequest::new(coverage))
        .snapshot
        .expect("a byte slice scan always observes EOF")
}

/// Alias used by callers that want the scan result rather than the convenience
/// whole-file snapshot.
pub fn scan(bytes: &[u8], request: ScanRequest) -> ScanResult {
    scan_bytes_with_request(bytes, request)
}

/// Return all raw records from a complete byte slice.
pub fn raw_line_records(bytes: &[u8]) -> Vec<RawLineRecord> {
    let snapshot = scan_bytes(bytes);
    snapshot.records.into_values().collect()
}

/// Normalize the whole file exactly as the tag algorithm does.
pub fn normalize_whole_file(bytes: &[u8]) -> Vec<u8> {
    normalize_for_tag(bytes)
}

/// Compute the tag for a complete raw byte buffer.
pub fn whole_file_tag(bytes: &[u8]) -> String {
    tag_for(bytes)
}

/// Capture a path using one descriptor per attempt. If the file identity,
/// metadata, or observed byte length changes during the first capture, retry
/// once with a new descriptor; if the second attempt is also inconsistent,
/// publish no snapshot and return [`CaptureError::Unstable`].
pub fn capture_path(
    path: impl AsRef<Path>,
    request: ScanRequest,
) -> Result<CoherentCapture, CaptureError> {
    let path = path.as_ref().to_path_buf();
    let mut last_unstable = None;
    for attempt in 1..=2 {
        match capture_path_once(&path, &request) {
            Ok(snapshot) => {
                return Ok(CoherentCapture {
                    snapshot,
                    attempts: attempt,
                });
            }
            Err(CaptureError::Unstable {
                before,
                after,
                observed_bytes,
            }) if attempt == 1 => {
                last_unstable = Some((before, after, observed_bytes));
            }
            Err(error) => return Err(error),
        }
    }
    let (before, after, observed_bytes) =
        last_unstable.expect("the retry loop records instability");
    Err(CaptureError::Unstable {
        before,
        after,
        observed_bytes,
    })
}

/// Capture only the published snapshot from a coherent path scan.
pub fn scan_path(path: impl AsRef<Path>, request: ScanRequest) -> Result<Snapshot, CaptureError> {
    capture_path(path, request).map(|capture| capture.snapshot)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoherentCapture {
    pub snapshot: Snapshot,
    pub attempts: u8,
}

#[derive(Debug)]
pub enum CaptureError {
    Io(io::Error),
    Unstable {
        before: CaptureProvenance,
        after: CaptureProvenance,
        observed_bytes: u64,
    },
}

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "hashline capture I/O error: {error}"),
            Self::Unstable {
                before,
                after,
                observed_bytes,
            } => write!(
                formatter,
                "hashline capture was unstable (before={before:?}, after={after:?}, observed_bytes={observed_bytes})"
            ),
        }
    }
}

impl std::error::Error for CaptureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Unstable { .. } => None,
        }
    }
}

impl From<io::Error> for CaptureError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ScanError> for CaptureError {
    fn from(error: ScanError) -> Self {
        match error {
            ScanError::Io(error) => Self::Io(error),
            ScanError::AlreadyFinished => Self::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "hashline scan finished before capture completed",
            )),
        }
    }
}

fn capture_path_once(path: &Path, request: &ScanRequest) -> Result<Snapshot, CaptureError> {
    let mut file = File::open(path)?;
    let before = CaptureProvenance::from_metadata(path.to_path_buf(), &file.metadata()?);
    let result = scan_reader_with_provenance(&mut file, request.clone(), before.clone())?;
    let after_descriptor = CaptureProvenance::from_metadata(path.to_path_buf(), &file.metadata()?);
    // A replacement through rename leaves the original descriptor stable, so
    // also inspect the current path entry without opening a second scan
    // descriptor. This catches path identity changes around the one forward
    // read while preserving the single-descriptor capture itself.
    let after_path =
        CaptureProvenance::from_metadata(path.to_path_buf(), &std::fs::metadata(path)?);
    let observed_bytes = result.coverage.byte_count;
    let stable = before.same_file_version(&after_descriptor)
        && before.same_file_version(&after_path)
        && before.byte_len == Some(observed_bytes)
        && after_descriptor.byte_len == Some(observed_bytes)
        && after_path.byte_len == Some(observed_bytes);
    if !stable {
        return Err(CaptureError::Unstable {
            before,
            after: after_path,
            observed_bytes,
        });
    }
    result.into_snapshot().ok_or(CaptureError::Unstable {
        before,
        after: after_path,
        observed_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::Value;
    use std::io::Cursor;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn raw_records_preserve_bom_terminators_and_interior_carriage_returns() {
        let bytes = b"\xEF\xBB\xBFfirst\r\nsecond\rinterior\nlast";
        let records = raw_line_records(bytes);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].content, b"\xEF\xBB\xBFfirst");
        assert_eq!(records[0].terminator, TerminatorKind::CrLf);
        assert_eq!(records[1].content, b"second\rinterior");
        assert_eq!(records[1].terminator, TerminatorKind::Lf);
        assert_eq!(records[2].content, b"last");
        assert_eq!(records[2].terminator, TerminatorKind::None);
    }

    #[test]
    fn scanner_bulk_copies_chunks_and_normalizes_once() {
        let input = b"\xef\xbb\xbfline with payload  \t\r\nnext\rrow\n".repeat(32_768);
        SCANNER_BYTE_PUSHES.with(|count| count.set(0));
        crate::hashline::oracle::xxhash32::NORMALIZATION_CALLS.with(|count| count.set(0));
        let mut scanner = ForwardScanner::new(ScanRequest::whole_file());
        for chunk in input.chunks(8_191) {
            scanner.push(chunk).unwrap();
        }
        let result = scanner.finish().unwrap();
        assert_eq!(
            SCANNER_BYTE_PUSHES.with(|count| count.get()),
            0,
            "per-byte vector pushes"
        );
        assert_eq!(
            crate::hashline::oracle::xxhash32::NORMALIZATION_CALLS.with(|count| count.get()),
            1,
            "snapshot normalization passes"
        );
        let snapshot = result.snapshot.unwrap();
        assert_eq!(snapshot.tag, tag_for(&input));
        let mut expected = ForwardScanner::new(ScanRequest::whole_file());
        legacy_push(&mut expected, &input);
        assert_eq!(snapshot, expected.finish().unwrap().snapshot.unwrap());
    }

    #[test]
    fn scanner_normalization_uses_retained_normalized_bytes() {
        let input = b"hello \t\r\nworld\t".repeat(32_768);
        crate::hashline::oracle::xxhash32::NORMALIZATION_CALLS.with(|count| count.set(0));
        let result = scan_bytes(&input);
        assert_eq!(
            crate::hashline::oracle::xxhash32::NORMALIZATION_CALLS.with(|count| count.get()),
            1,
            "snapshot normalization passes"
        );
        assert_eq!(result.tag, tag_for(&input));
    }

    // Frozen bytewise scanner for output parity at chunk boundaries.
    pub(super) fn legacy_push(scanner: &mut ForwardScanner, bytes: &[u8]) {
        for &byte in bytes {
            SCANNER_BYTE_PUSHES.with(|count| count.set(count.get() + 1));
            scanner.raw_bytes.push(byte);
            if byte == b'\n' {
                let mut content = std::mem::take(&mut scanner.current_line);
                let terminator = if content.last() == Some(&b'\r') {
                    content.pop();
                    TerminatorKind::CrLf
                } else {
                    TerminatorKind::Lf
                };
                scanner.scanned_line_count += 1;
                let line_number = scanner.next_line_number;
                scanner.next_line_number += 1;
                let byte_end = scanner.raw_bytes.len() as u64;
                scanner.retain_line_if_requested(
                    line_number,
                    RawLineRecord::new(content, terminator),
                    scanner.current_line_start,
                    byte_end,
                );
                scanner.current_line_start = byte_end;
            } else {
                SCANNER_BYTE_PUSHES.with(|count| count.set(count.get() + 1));
                scanner.current_line.push(byte);
            }
        }
    }

    #[test]
    fn a_trailing_terminator_does_not_create_an_extra_record() {
        assert_eq!(raw_line_records(b"one\n").len(), 1);
        assert_eq!(raw_line_records(b"one\r\n").len(), 1);
        assert!(raw_line_records(b"").is_empty());
    }

    #[test]
    fn normalized_tag_ignores_only_trailing_space_tab_and_cr() {
        let lf = b"alpha\nbeta\n";
        let decorated = b"alpha \t\r\nbeta\r\n";
        assert_eq!(normalize_whole_file(decorated), normalize_whole_file(lf));
        assert_eq!(whole_file_tag(decorated), whole_file_tag(lf));
        assert_eq!(normalize_whole_file(b"a\rreturn\n"), b"a\rreturn\n");
        assert_eq!(normalize_whole_file(b"a  \t\r"), b"a");
    }

    #[test]
    fn ranged_retention_uses_the_same_raw_parser_as_whole_file_retention() {
        let bytes = b"zero\r\none\ntwo\rthree\n";
        let whole = scan_bytes(bytes);
        let ranged = scan_bytes_with_request(bytes, ScanRequest::for_range(2, 2));
        let ranged_snapshot = ranged.snapshot.unwrap();
        assert_eq!(ranged_snapshot.raw_record(2), whole.raw_record(2));
        assert_eq!(ranged_snapshot.coverage.scanned_line_count, 3);
        assert!(!ranged_snapshot.is_seen(1));
        assert!(!ranged_snapshot.is_seen(3));
        assert_eq!(ranged_snapshot.total_lines, whole.total_lines);
        assert_eq!(ranged_snapshot.tag, whole.tag);
    }

    #[test]
    fn scan_only_rows_are_not_seen_or_eligible() {
        let result = scan_bytes_with_request(b"a\nb\nc\n", ScanRequest::for_lines([2]));
        let snapshot = result.snapshot.unwrap();
        assert_eq!(snapshot.coverage.scanned_line_count, 3);
        assert_eq!(snapshot.coverage.seen_lines, BTreeSet::from([2]));
        assert!(snapshot.coverage.is_eligible(2));
        assert!(!snapshot.coverage.is_eligible(3));
        assert!(snapshot.raw_record(3).is_none());
    }

    #[test]
    fn no_snapshot_is_published_before_eof() {
        let mut scanner = ForwardScanner::new(ScanRequest::for_lines([1]));
        scanner.push(b"first\nsecond").unwrap();
        assert!(!scanner.eof_observed());
        assert!(scanner.published_snapshot().is_none());
        let result = scanner.finish().unwrap();
        assert!(result.coverage.eof_observed);
        assert!(scanner.published_snapshot().is_some());
        assert!(scanner.push(b"after eof").is_err());
    }

    #[test]
    fn reader_scan_is_forward_and_publishes_at_eof() {
        let mut reader = Cursor::new(b"one\r\ntwo\nthree".to_vec());
        let result = scan_reader(&mut reader, ScanRequest::for_lines([2])).unwrap();
        let snapshot = result.snapshot.unwrap();
        assert_eq!(
            snapshot.raw_record(2).unwrap().terminator,
            TerminatorKind::Lf
        );
        assert_eq!(snapshot.total_lines, 3);
        assert!(snapshot.eof_observed());
    }

    #[test]
    fn path_capture_exposes_provenance_and_observes_eof() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"one\ntwo").unwrap();
        file.flush().unwrap();
        let request = ScanRequest::for_lines([2])
            .with_provenance(CaptureProvenance::default().with_label("reader", "test"));
        let capture = capture_path(file.path(), request).unwrap();
        assert_eq!(capture.attempts, 1);
        assert_eq!(capture.snapshot.total_lines, 2);
        assert_eq!(capture.snapshot.provenance.labels["reader"], "test");
        assert_eq!(capture.snapshot.capture_provenance.byte_len, Some(7));
    }

    #[test]
    fn oracle_corpus_byte_model_rows() {
        // This test covers LF, CRLF, mixed-terminator, BOM, empty-input,
        // missing-final-newline, Unicode, and trailing-whitespace cases,
        // including the expected-rejection version of each. Tests for
        // BOF/EOF addressing, one-line and empty-boundary inputs, blocks,
        // repairs, registers, and known deviations stay in their respective
        // test groups.
        const OWNED: &[&str] = &[
            "lf",
            "lf-rejection",
            "crlf",
            "crlf-rejection",
            "mixed-terminators",
            "mixed-terminators-rejection",
            "bom",
            "bom-rejection",
            "empty",
            "empty-rejection",
            "missing-final-newline",
            "missing-final-newline-rejection",
            "unicode",
            "unicode-rejection",
            "trailing-whitespace",
            "trailing-whitespace-rejection",
        ];
        const DEFERRED: &[&str] = &[
            "bof",
            "bof-rejection",
            "eof",
            "eof-rejection",
            "eof-relative",
            "eof-relative-rejection",
            "one-line",
            "one-line-rejection",
            "empty-boundary",
            "empty-boundary-rejection",
            "block",
            "block-rejection",
            "repair",
            "repair-negative-control",
            "registered-deviation",
            "registered-deviation-negative-control",
            "named-register",
            "anonymous-register",
            "cross-file-register",
            "register-overflow",
        ];

        let mut consumed = 0;
        let mut deferred = 0;
        for line in include_str!("../oracle/fixtures.jsonl").lines() {
            let row: Value = serde_json::from_str(line).expect("oracle fixture JSON must parse");
            let category = row["fixture_category"]
                .as_str()
                .expect("oracle fixture category must be a string");
            if !OWNED.contains(&category) {
                assert!(
                    DEFERRED.contains(&category),
                    "new oracle category {category:?} needs an explicit slice owner"
                );
                deferred += 1;
                continue;
            }
            consumed += 1;

            let bytes = base64::engine::general_purpose::STANDARD
                .decode(row["initial_base64"].as_str().unwrap())
                .expect("oracle fixture initial_base64 must decode");
            let snapshot = scan_bytes(&bytes);
            let expected_tag = row["snapshot_tag"].as_str().unwrap();
            assert_eq!(snapshot.tag, expected_tag, "fixture {}", row["id"]);
            assert_eq!(snapshot.tag, tag_for(&bytes), "fixture {}", row["id"]);

            let family = category.strip_suffix("-rejection").unwrap_or(category);
            let expected_terminators: Vec<TerminatorKind> = match family {
                "lf" | "unicode" => vec![TerminatorKind::Lf; 3],
                "bom" => vec![TerminatorKind::Lf; 2],
                "crlf" => vec![TerminatorKind::CrLf; 3],
                "mixed-terminators" => vec![
                    TerminatorKind::Lf,
                    TerminatorKind::CrLf,
                    TerminatorKind::None,
                ],
                "empty" => Vec::new(),
                "missing-final-newline" => vec![TerminatorKind::Lf, TerminatorKind::None],
                "trailing-whitespace" => vec![TerminatorKind::Lf, TerminatorKind::Lf],
                _ => unreachable!("owned category must have a byte-model shape"),
            };
            let records: Vec<&RawLineRecord> = snapshot.records.values().collect();
            assert_eq!(
                records.len(),
                expected_terminators.len(),
                "fixture {}",
                row["id"]
            );
            assert_eq!(
                snapshot.total_lines,
                expected_terminators.len(),
                "fixture {}",
                row["id"]
            );
            assert_eq!(
                records
                    .iter()
                    .map(|record| record.terminator)
                    .collect::<Vec<_>>(),
                expected_terminators,
                "fixture {}",
                row["id"]
            );
            let reconstructed: Vec<u8> = records
                .iter()
                .flat_map(|record| record.to_bytes())
                .collect();
            assert_eq!(reconstructed, bytes, "fixture {}", row["id"]);
        }

        assert_eq!(
            consumed, 64,
            "the owned byte-model corpus must remain complete"
        );
        assert_eq!(
            deferred, 64,
            "the deferred corpus rows must remain explicitly classified"
        );
    }
}
