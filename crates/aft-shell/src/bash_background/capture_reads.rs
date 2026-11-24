//! Bounded reads of a task's output capture for the tool display's detail and
//! for completion-notice previews.
//!
//! Both used to read every stream whole (up to the 100 MiB disk cap) so that
//! masking ran over contiguous text. The readers here keep masking exactly as
//! strong while holding only part of the capture in memory. They rely on how
//! the masker matches (see the notes above `json_secret_field_spans` in
//! `log_redact.rs`): a match of every format pattern, and of every exact value
//! without a newline, lies within one line. Only a JSON secret field, a
//! private-key block and an exact value that contains a newline can cross a
//! newline, so each reader cuts at newlines and checks those three at its cuts.

use std::borrow::Cow;

use crate::bash_background::buffer::{BgBuffer, StreamKind, DISK_LIMIT_BYTES};
use crate::bash_background::persistence::BgMode;
use crate::log_redact::SecretValues;

/// Bytes read past the display detail's limit to finish the line that crosses it.
const LINE_FINISH_BYTES: u64 = 1024 * 1024;

#[cfg(any(test, feature = "test-support"))]
pub const DISPLAY_LINE_FINISH_BYTES: u64 = LINE_FINISH_BYTES;

/// Bytes read per step while scanning a capture for credentials.
const SCAN_CHUNK_BYTES: u64 = 1024 * 1024;

/// A line longer than this is not split into windows: the caller falls back to
/// reading the whole capture, as it did before these readers existed.
const MAX_WINDOW_LINE_BYTES: usize = 8 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    static SCAN_CHUNK_FOR_TEST: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

fn scan_chunk_bytes() -> u64 {
    #[cfg(test)]
    if let Some(bytes) = SCAN_CHUNK_FOR_TEST.with(std::cell::Cell::get) {
        return bytes;
    }
    SCAN_CHUNK_BYTES
}

/// Scan in `bytes`-sized steps on this thread, to put chunk edges inside test
/// output.
#[cfg(test)]
pub(crate) fn set_scan_chunk_for_test(bytes: Option<u64>) {
    SCAN_CHUNK_FOR_TEST.with(|cell| cell.set(bytes));
}

/// The starts of a task's streams for a display detail that shows at most
/// `limit` bytes of each.
pub(crate) struct DisplayStreams {
    pub stdout: String,
    pub stderr: String,
    /// Capture bytes past the cut that were never read.
    pub unread: u64,
}

/// Read the start of each stream for the display detail. A stream that fits
/// in `limit` plus [`LINE_FINISH_BYTES`] is read whole. A longer one is cut
/// after the newline that ends the line crossing `limit` (or, when that line is
/// longer than [`LINE_FINISH_BYTES`], before it), so every kept line is whole
/// and masks as it would in the full text. The stream is read whole instead
/// when the cut could split a credential the masker only recognizes intact: a
/// JSON secret value still open at the cut, or any secret value with a newline
/// (`multiline_secrets`).
///
/// `None` when a stream is unreadable or over the disk cap, where the
/// whole-capture read this replaces also returned nothing.
pub(crate) fn read_display_streams(
    buffer: &BgBuffer,
    mode: &BgMode,
    limit: u64,
    multiline_secrets: bool,
) -> Option<DisplayStreams> {
    let (stdout, stdout_unread) =
        read_display_stream(buffer, StreamKind::Stdout, limit, multiline_secrets)?;
    let (stderr, stderr_unread) = if *mode == BgMode::Pty {
        // The pseudoterminal capture is one stream; both kinds name it.
        (String::new(), 0)
    } else {
        read_display_stream(buffer, StreamKind::Stderr, limit, multiline_secrets)?
    };
    Some(DisplayStreams {
        stdout,
        stderr,
        unread: stdout_unread.saturating_add(stderr_unread),
    })
}

pub(crate) struct MaskedDisplay {
    pub streams: (String, String),
    pub unread: u64,
    pub tail: Option<serde_json::Value>,
}

/// Read and mask a task's output for its display detail. The capture is read
/// before the task's secrets are locked for masking, so no file read holds
/// that lock. `None` when no masking is known to be safe for the task (after a
/// restart that lost its secrets) or the capture could not be read.
pub(crate) fn masked_display_streams(
    task: &super::super::BgTask,
    metadata: &crate::bash_background::persistence::PersistedTask,
    buffer: &BgBuffer,
) -> Option<MaskedDisplay> {
    let safe = |masker: Option<&SecretValues>| {
        masker.is_some() || metadata.has_safe_completion_redaction_after_restart()
    };
    let multiline_secrets = {
        let secrets = task.completion_secrets.lock().ok();
        let masker = secrets.as_deref().and_then(Option::as_ref);
        if !safe(masker) {
            return None;
        }
        masker.is_some_and(SecretValues::has_multiline_value)
    };
    let limit = crate::bash_background::frames::bash_detail_byte_limit();
    let read = read_display_streams(buffer, &metadata.mode, limit, multiline_secrets)?;
    let secrets = task.completion_secrets.lock().ok();
    let masker = secrets.as_deref().and_then(Option::as_ref);
    // The secrets can only have been dropped since the first check; a value
    // with a newline the read did not plan for withholds the output.
    if !safe(masker)
        || (!multiline_secrets && masker.is_some_and(SecretValues::has_multiline_value))
    {
        return None;
    }
    let mask = |text: &str| -> Option<String> {
        let masked = masker.map_or_else(
            || crate::log_redact::aft_redactor(text),
            |masker| masker.redact(text),
        );
        crate::log_redact::redact_ticket_digest(
            &masked,
            metadata.completion_ticket_digest.as_deref(),
        )
        .ok()
        .map(|text| text.into_owned())
    };
    let stdout = mask(&read.stdout)?;
    let stderr = mask(&read.stderr)?;
    // A bounded detail prefix is not the end of the output.
    let tail = (read.unread == 0)
        .then(|| crate::bash_background::frames::bash_tail(metadata, &stdout, &stderr, &mask))
        .flatten();
    Some(MaskedDisplay {
        streams: (stdout, stderr),
        unread: read.unread,
        tail,
    })
}

fn read_display_stream(
    buffer: &BgBuffer,
    stream: StreamKind,
    limit: u64,
    multiline_secrets: bool,
) -> Option<(String, u64)> {
    let len = buffer.stream_len_checked(stream).ok()?;
    if len > DISK_LIMIT_BYTES {
        return None;
    }
    let wanted = limit.saturating_add(LINE_FINISH_BYTES);
    if len <= wanted || multiline_secrets {
        let bytes = buffer.read_stream_bytes(stream, 0, len).ok()?;
        return Some((String::from_utf8_lossy(&bytes).into_owned(), 0));
    }
    let mut bytes = buffer.read_stream_bytes(stream, 0, wanted).ok()?;
    let limit = limit as usize;
    let cut = match bytes[limit.min(bytes.len())..]
        .iter()
        .position(|byte| *byte == b'\n')
    {
        Some(offset) => limit + offset + 1,
        None => bytes[..limit.min(bytes.len())]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |offset| offset + 1),
    };
    bytes.truncate(cut);
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if crate::log_redact::json_secret_value_open_at_end(&text, text.len()) {
        let bytes = buffer.read_stream_bytes(stream, 0, len).ok()?;
        return Some((String::from_utf8_lossy(&bytes).into_owned(), 0));
    }
    Some((text, len - cut as u64))
}

/// A task's pipe capture as the completion preview masks it: stdout, a newline
/// when both streams have output, then stderr, as `combine_streams` joins them.
struct Combined<'a> {
    buffer: &'a BgBuffer,
    stdout: u64,
    stderr: u64,
}

impl<'a> Combined<'a> {
    /// `None` when a stream is unreadable or over the disk cap, the cases where
    /// the whole-capture read returned nothing.
    fn open(buffer: &'a BgBuffer) -> Option<Self> {
        let stdout = buffer.stream_len_checked(StreamKind::Stdout).ok()?;
        let stderr = buffer.stream_len_checked(StreamKind::Stderr).ok()?;
        (stdout <= DISK_LIMIT_BYTES && stderr <= DISK_LIMIT_BYTES).then_some(Self {
            buffer,
            stdout,
            stderr,
        })
    }

    fn separator(&self) -> u64 {
        u64::from(self.stdout > 0 && self.stderr > 0)
    }

    fn len(&self) -> u64 {
        self.stdout + self.separator() + self.stderr
    }

    fn read(&self, start: u64, len: u64) -> Option<Vec<u8>> {
        let end = start.saturating_add(len).min(self.len());
        let mut bytes = Vec::new();
        if start < self.stdout {
            let to = end.min(self.stdout);
            bytes.extend(
                self.buffer
                    .read_stream_bytes(StreamKind::Stdout, start, to - start)
                    .ok()?,
            );
        }
        if self.separator() == 1 && start <= self.stdout && self.stdout < end {
            bytes.push(b'\n');
        }
        let stderr_start = self.stdout + self.separator();
        if end > stderr_start {
            let from = start.max(stderr_start);
            bytes.extend(
                self.buffer
                    .read_stream_bytes(StreamKind::Stderr, from - stderr_start, end - from)
                    .ok()?,
            );
        }
        // A capture that shrank since it was measured cannot be scanned in
        // step with its offsets.
        (bytes.len() as u64 == end.saturating_sub(start)).then_some(bytes)
    }
}

/// Decode and normalize whole lines exactly as the preview normalizes the
/// whole capture. Normalization keeps no state across a newline.
fn window_text(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    super::super::normalize_piped_display_output(&mut text);
    text
}

/// Whether masking a window of whole lines changes it, or a credential that
/// crosses a newline may cross the window's edge.
fn window_needs_mask(text: &str, secrets: &SecretValues) -> bool {
    matches!(secrets.redact(text), Cow::Owned(_))
        || crate::log_redact::has_private_key_marker(text)
        || crate::log_redact::json_secret_value_open_at_end(text, text.len())
        || crate::log_redact::json_secret_label_open_at_end(text)
}

/// Whether masking the whole normalized capture with `secrets` could change it,
/// read one window of whole lines at a time. A window ends after a newline, so
/// a match that cannot contain a newline lies inside one window. A match that
/// can (a JSON secret field, a private-key block) is caught whenever it touches
/// a window's end, and a secret value containing a newline answers yes without
/// reading. A line over [`MAX_WINDOW_LINE_BYTES`] also answers yes, so the
/// caller masks the whole capture as before.
///
/// `None` when the capture is unreadable or over the disk cap.
pub(crate) fn capture_needs_full_mask(buffer: &BgBuffer, secrets: &SecretValues) -> Option<bool> {
    let capture = Combined::open(buffer)?;
    if secrets.has_multiline_value() {
        return Some(true);
    }
    let mut offset = 0;
    let mut pending: Vec<u8> = Vec::new();
    while offset < capture.len() {
        let block = capture.read(offset, scan_chunk_bytes())?;
        offset += block.len() as u64;
        pending.extend_from_slice(&block);
        let end = if offset >= capture.len() {
            pending.len()
        } else {
            match pending.iter().rposition(|byte| *byte == b'\n') {
                Some(newline) => newline + 1,
                None if pending.len() > MAX_WINDOW_LINE_BYTES => return Some(true),
                None => continue,
            }
        };
        if window_needs_mask(&window_text(&pending[..end]), secrets) {
            return Some(true);
        }
        pending.drain(..end);
    }
    Some(false)
}

/// The start and end of the normalized capture that ticket masking compares
/// with a rendered preview.
pub(crate) enum RawEdges {
    /// The whole normalized capture, which was short enough to keep.
    Whole(String),
    /// At least `width` bytes from each end, each a whole run of lines with
    /// more of the capture beyond it.
    Ends { head: String, tail: String },
}

/// Read only the first and last `width`-plus bytes of the normalized capture.
/// `None` when a line over [`MAX_WINDOW_LINE_BYTES`] is in the way or the
/// capture is unreadable; the caller then reads it whole.
pub(crate) fn read_raw_edges(buffer: &BgBuffer, width: usize) -> Option<RawEdges> {
    // A char is at most four bytes, so a slice cut at `width` on a char
    // boundary from text this long always leaves text beyond the cut.
    let enough = width + 4;
    let capture = Combined::open(buffer)?;
    let chunk = scan_chunk_bytes();
    let mut head = String::new();
    let mut offset = 0;
    let mut pending: Vec<u8> = Vec::new();
    while head.len() <= enough {
        if offset >= capture.len() {
            head.push_str(&window_text(&pending));
            return Some(RawEdges::Whole(head));
        }
        let block = capture.read(offset, chunk)?;
        offset += block.len() as u64;
        pending.extend_from_slice(&block);
        if offset < capture.len() {
            let Some(newline) = pending.iter().rposition(|byte| *byte == b'\n') else {
                if pending.len() > MAX_WINDOW_LINE_BYTES {
                    return None;
                }
                continue;
            };
            head.push_str(&window_text(&pending[..=newline]));
            pending.drain(..=newline);
        }
    }
    let mut tail = String::new();
    let mut cursor = capture.len();
    let mut pending: Vec<u8> = Vec::new();
    while tail.len() <= enough {
        let start = cursor.saturating_sub(chunk);
        let mut block = capture.read(start, cursor - start)?;
        block.extend_from_slice(&pending);
        pending = block;
        cursor = start;
        let line_start = if cursor == 0 {
            0
        } else {
            match pending.iter().position(|byte| *byte == b'\n') {
                Some(newline) => newline + 1,
                None if pending.len() > MAX_WINDOW_LINE_BYTES => return None,
                None => continue,
            }
        };
        let lines = pending.split_off(line_start);
        tail.insert_str(0, &window_text(&lines));
        if cursor == 0 && pending.is_empty() {
            // The tail reached the start: it is the whole capture.
            return Some(RawEdges::Whole(tail));
        }
    }
    let head_end = crate::compress::generic::ceil_char_boundary(&head, width);
    head.truncate(head_end);
    let tail_start =
        crate::compress::generic::floor_char_boundary(&tail, tail.len().saturating_sub(width));
    tail.drain(..tail_start);
    Some(RawEdges::Ends { head, tail })
}

#[cfg(test)]
#[path = "capture_reads_tests.rs"]
pub(crate) mod tests;
