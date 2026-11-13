//! Reading large files in fixed steps instead of reading each file whole.
//!
//! Search and grep used to read every candidate file whole, on every search
//! thread at once, so a few growing multi-hundred-megabyte logs gave
//! gigabyte memory bursts. The readers that account for those bytes (the
//! literal presence check and the grep scan sites) now read a large file in
//! steps of [`STEP_BYTES`] and run their unchanged per-match logic on each
//! step, so their answers are the same as a whole read's.
//!
//! A file is large when its size at open exceeds [`LARGE_READ_BYTES`]. Smaller
//! files keep the whole-read code paths.
//!
//! This module holds the step reading, the incremental UTF-8 check, the
//! streamed phrase normalizer, the streamed line scan, the line-locality test
//! that decides whether a pattern may be scanned in steps, and the per-site
//! large-read counters that health reports.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use regex::bytes::Regex;

/// Files larger than this are read in steps where a site supports it, and
/// counted in health as large reads.
pub(crate) const LARGE_READ_BYTES: u64 = 4 * 1024 * 1024;

/// Bytes read per step from a large file.
pub(crate) const STEP_BYTES: usize = 1024 * 1024;

#[cfg(test)]
thread_local! {
    /// Replaces [`STEP_BYTES`] on this thread, so tests can put step edges
    /// anywhere in small generated files.
    static STEP_OVERRIDE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// Replaces [`LARGE_READ_BYTES`] on this thread, so small generated files
    /// take the streamed paths.
    static LARGE_OVERRIDE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    /// Runs after every step a [`Steps`] reads on this thread, with the bytes
    /// read so far, so a test can cancel or outrun a deadline mid-file.
    static STEP_TAP: std::cell::RefCell<Option<Box<dyn FnMut(u64)>>> =
        const { std::cell::RefCell::new(None) };
    /// A [`Steps`] reservation that would grow its buffer past this many
    /// bytes fails on this thread, as a refused allocation does.
    static REFUSE_RESERVE_OVER: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
}

/// Run `work` with `tap` called after every step read on this thread.
#[cfg(test)]
pub(crate) fn with_step_tap<T>(tap: impl FnMut(u64) + 'static, work: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            STEP_TAP.with(|cell| *cell.borrow_mut() = None);
        }
    }
    STEP_TAP.with(|cell| *cell.borrow_mut() = Some(Box::new(tap)));
    let _reset = Reset;
    work()
}

/// Run `work` with every step buffer reservation past `bytes` refused.
#[cfg(test)]
pub(crate) fn with_reserve_limit<T>(bytes: usize, work: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            REFUSE_RESERVE_OVER.with(|cell| cell.set(usize::MAX));
        }
    }
    REFUSE_RESERVE_OVER.with(|cell| cell.set(bytes));
    let _reset = Reset;
    work()
}

/// Run `work` with a different step size and large-file threshold on this
/// thread.
#[cfg(test)]
pub(crate) fn with_test_steps<T>(step: usize, large: u64, work: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            STEP_OVERRIDE.with(|cell| cell.set(None));
            LARGE_OVERRIDE.with(|cell| cell.set(None));
        }
    }
    STEP_OVERRIDE.with(|cell| cell.set(Some(step.max(1))));
    LARGE_OVERRIDE.with(|cell| cell.set(Some(large)));
    let _reset = Reset;
    work()
}

pub(crate) fn step_bytes() -> usize {
    #[cfg(test)]
    if let Some(step) = STEP_OVERRIDE.with(std::cell::Cell::get) {
        return step;
    }
    STEP_BYTES
}

/// Whether a file of `size` bytes is a large read.
pub(crate) fn is_large(size: u64) -> bool {
    #[cfg(test)]
    if let Some(large) = LARGE_OVERRIDE.with(std::cell::Cell::get) {
        return size > large;
    }
    size > LARGE_READ_BYTES
}

// ---------------------------------------------------------------------------
// Step reading

/// A source of file bytes read in steps: an opened file, or a test's bytes.
pub(crate) trait StepSource {
    /// Append at most `size` more bytes to `buffer`. Fewer bytes than asked
    /// is not the end of the file; only an append of no bytes is.
    fn append(&mut self, buffer: Vec<u8>, size: usize) -> io::Result<Vec<u8>>;
}

impl StepSource for crate::bounded_io::Reader {
    fn append(&mut self, buffer: Vec<u8>, size: usize) -> io::Result<Vec<u8>> {
        crate::bounded_io::Reader::append(self, buffer, size)
    }
}

/// Reads a source in steps up to the size the file had when it was opened,
/// so bytes appended by a writer during the read are not read.
pub(crate) struct Steps<'a, S: StepSource + ?Sized> {
    source: &'a mut S,
    remaining: u64,
    read: u64,
    step: usize,
}

impl<'a, S: StepSource + ?Sized> Steps<'a, S> {
    /// `remaining` is the size at open less the bytes already read.
    pub(crate) fn new(source: &'a mut S, remaining: u64) -> Self {
        Self {
            source,
            remaining,
            read: 0,
            step: step_bytes(),
        }
    }

    /// Bytes read so far.
    pub(crate) fn read(&self) -> u64 {
        self.read
    }

    /// Append the next step to `buffer`; the returned count is 0 at the end.
    /// `buffer` has room for two steps, which holds a step and the partial
    /// line carried before it. A line longer than that grows it once, to
    /// everything the rest of the file could need: one copy of the carried
    /// bytes instead of one per doubling. The carried bytes were read from
    /// this file and are not yet dropped, so the buffer never holds more than
    /// the file's size at open: no more than a whole read reserves. Only the
    /// bytes the line fills are touched.
    ///
    /// The reservation is exact and fallible: an allocation the system
    /// refuses is the same `io::Error::other` a whole read's
    /// `try_reserve_exact` gives, and the caller skips the file as it skips
    /// one whose whole read failed. The source then reads into the reserved
    /// room only (see [`crate::bounded_io::Reader::append`]).
    pub(crate) fn append(&mut self, buffer: &mut Vec<u8>) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let remaining = usize::try_from(self.remaining).unwrap_or(usize::MAX);
        let want = remaining.min(self.step);
        if buffer.capacity() - buffer.len() < want {
            let limit = buffer.len().saturating_add(remaining);
            let needed = buffer.len() + want;
            let two_steps = self.step.saturating_mul(2);
            let target = if needed <= two_steps {
                two_steps.min(limit)
            } else {
                limit
            };
            #[cfg(test)]
            if REFUSE_RESERVE_OVER.with(std::cell::Cell::get) < target {
                return Err(io::Error::other("test refused the reservation"));
            }
            buffer
                .try_reserve_exact(target - buffer.len())
                .map_err(io::Error::other)?;
        }
        let before = buffer.len();
        *buffer = self.source.append(std::mem::take(buffer), want)?;
        #[cfg(test)]
        STEP_TAP.with(|tap| {
            if let Some(tap) = tap.borrow_mut().as_mut() {
                tap(self.read + (buffer.len() - before) as u64);
            }
        });
        let read = buffer.len() - before;
        self.read += read as u64;
        if read == 0 {
            // The file is shorter than it was at open.
            self.remaining = 0;
        } else {
            self.remaining = self.remaining.saturating_sub(read as u64);
        }
        Ok(read)
    }
}

// ---------------------------------------------------------------------------
// Incremental UTF-8 validation

/// Validates UTF-8 across steps, carrying an incomplete character between
/// them. The bytes are valid UTF-8 exactly when every feed returns true and
/// [`Utf8Check::finish`] does too, as `std::str::from_utf8` over all of them
/// would decide.
#[derive(Default)]
pub(crate) struct Utf8Check {
    carry: [u8; 4],
    carry_len: usize,
}

impl Utf8Check {
    /// Check the next bytes, handing every complete stretch of text to
    /// `text` in order. False when the bytes cannot be UTF-8.
    pub(crate) fn feed(&mut self, mut bytes: &[u8], mut text: impl FnMut(&str)) -> bool {
        if self.carry_len > 0 {
            // The carry is a valid start of one character; complete it.
            let width = utf8_width(self.carry[0]);
            let take = (width - self.carry_len).min(bytes.len());
            self.carry[self.carry_len..self.carry_len + take].copy_from_slice(&bytes[..take]);
            self.carry_len += take;
            bytes = &bytes[take..];
            match std::str::from_utf8(&self.carry[..self.carry_len]) {
                Ok(character) => {
                    text(character);
                    self.carry_len = 0;
                }
                Err(error) if error.error_len().is_none() => return true,
                Err(_) => return false,
            }
        }
        match std::str::from_utf8(bytes) {
            Ok(valid) => {
                text(valid);
                true
            }
            Err(error) => {
                if error.error_len().is_some() {
                    return false;
                }
                let valid = error.valid_up_to();
                if valid > 0 {
                    // `valid_up_to` marks the end of a valid prefix.
                    text(std::str::from_utf8(&bytes[..valid]).unwrap_or_default());
                }
                let rest = &bytes[valid..];
                self.carry[..rest.len()].copy_from_slice(rest);
                self.carry_len = rest.len();
                true
            }
        }
    }

    /// True when no incomplete character is left at the end.
    pub(crate) fn finish(&self) -> bool {
        self.carry_len == 0
    }
}

/// Byte length of the character a valid UTF-8 lead byte starts.
fn utf8_width(lead: u8) -> usize {
    match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

// ---------------------------------------------------------------------------
// Streamed exact-phrase normalization and presence

/// [`crate::commands::semantic_search::exact_lane::normalize_exact_phrase`]
/// over text that arrives in pieces: words (split on Unicode White_Space, as
/// `str::split_whitespace` splits) joined by single spaces, ASCII-lowercased.
/// A space is written only once the next word starts, so there is never a
/// leading or trailing one.
#[derive(Default)]
pub(crate) struct PhraseNormalizer {
    seen_word: bool,
    pending_space: bool,
}

impl PhraseNormalizer {
    pub(crate) fn push(&mut self, text: &str, out: &mut Vec<u8>) {
        let bytes = text.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte < 0x80 {
                // The ASCII White_Space characters are U+0009..=U+000D and
                // U+0020 (`u8::is_ascii_whitespace` leaves out U+000B).
                if matches!(byte, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ') {
                    self.space();
                } else {
                    self.word(out);
                    out.push(byte.to_ascii_lowercase());
                }
                index += 1;
                continue;
            }
            let character = text[index..].chars().next().unwrap_or_default();
            let width = character.len_utf8();
            if character.is_whitespace() {
                self.space();
            } else {
                self.word(out);
                out.extend_from_slice(&bytes[index..index + width]);
            }
            index += width;
        }
    }

    fn space(&mut self) {
        if self.seen_word {
            self.pending_space = true;
        }
    }

    fn word(&mut self, out: &mut Vec<u8>) {
        if self.pending_space {
            out.push(b' ');
            self.pending_space = false;
        }
        self.seen_word = true;
    }
}

/// Why a streamed read stopped before the end of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Interrupted {
    /// The phase's time limit passed.
    Deadline,
    /// The request was cancelled.
    Cancelled,
}

/// What a streamed presence check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresenceRead {
    /// The file is UTF-8 text; whether its normalized text holds the phrase.
    Holds(bool),
    /// The file could not be read, or is binary or not UTF-8.
    Unreadable,
    /// The check stopped before the end of the file.
    Interrupted(Interrupted),
}

/// Whether the file `source` reads holds `needle` (an already normalized
/// phrase) in its normalized text, with the whole-file answer: binary bytes
/// in the first 1,024 and invalid UTF-8 anywhere make the file unreadable,
/// even after the phrase was found. `checkpoint` runs before every step and
/// can stop the check. Also returns the bytes read.
pub(crate) fn stream_presence(
    source: &mut (impl StepSource + ?Sized),
    size_at_open: u64,
    needle: &[u8],
    checkpoint: impl FnMut() -> Option<Interrupted>,
) -> (PresenceRead, u64) {
    let mut steps = Steps::new(source, size_at_open);
    let answer = presence_steps(&mut steps, size_at_open, needle, checkpoint);
    (answer, steps.read())
}

fn presence_steps<S: StepSource + ?Sized>(
    steps: &mut Steps<'_, S>,
    size_at_open: u64,
    needle: &[u8],
    mut checkpoint: impl FnMut() -> Option<Interrupted>,
) -> PresenceRead {
    let mut step = Vec::new();
    // The binary sniff sees the same first bytes a whole read's would.
    // `is_binary_bytes` is `content_inspector::inspect`, which looks at the
    // first 1,024 bytes only: content_inspector 0.2.4, src/lib.rs line 35
    // (`MAX_SCAN_SIZE = 1024`) and line 120 (`min(buffer.len(),
    // MAX_SCAN_SIZE)`); its byte-order marks and magic numbers are prefix
    // checks of at most 4 bytes. So these bytes decide it as the whole file
    // would.
    let sniff = usize::try_from(size_at_open.min(1024)).unwrap_or(1024);
    let mut ended = false;
    while step.len() < sniff {
        if let Some(interrupted) = checkpoint() {
            return PresenceRead::Interrupted(interrupted);
        }
        match steps.append(&mut step) {
            Ok(0) => {
                ended = true;
                break;
            }
            Ok(_) => {}
            Err(_) => return PresenceRead::Unreadable,
        }
    }
    if crate::search_index::is_binary_bytes(&step) {
        return PresenceRead::Unreadable;
    }
    let finder = memchr::memmem::Finder::new(needle);
    // The normalized stream's last `needle.len() - 1` bytes, then the
    // current step's normalized bytes, so a phrase across steps is found.
    let keep = needle.len().saturating_sub(1);
    let mut window = Vec::new();
    let mut found = needle.is_empty();
    let mut normalizer = PhraseNormalizer::default();
    let mut utf8 = Utf8Check::default();
    loop {
        let valid = utf8.feed(&step, |text| {
            if !found {
                normalizer.push(text, &mut window);
            }
        });
        if !valid {
            return PresenceRead::Unreadable;
        }
        if !found {
            found = finder.find(&window).is_some();
            if window.len() > keep {
                window.drain(..window.len() - keep);
            }
        }
        if ended {
            break;
        }
        if let Some(interrupted) = checkpoint() {
            return PresenceRead::Interrupted(interrupted);
        }
        step.clear();
        match steps.append(&mut step) {
            Ok(0) => ended = true,
            Ok(_) => {}
            Err(_) => return PresenceRead::Unreadable,
        }
    }
    if !utf8.finish() {
        return PresenceRead::Unreadable;
    }
    PresenceRead::Holds(found)
}

// ---------------------------------------------------------------------------
// Streamed line scans

/// What happened to one match occurrence handed to a scan's per-match logic.
pub(crate) enum MatchVisit {
    /// The scan must end: a stop flag, the result cap, cancellation or the
    /// deadline.
    Stop,
    /// The occurrence is on a line that already produced a match.
    SameLine,
    /// The occurrence was the first on its line; skipping to the next line
    /// avoids searching the rest of that line again.
    Recorded { next_line_start: usize },
}

/// Where a scan's per-match logic gets the line and text of a match, so the
/// same logic runs over a whole buffer and over a file read in steps.
pub(crate) trait ScanLines {
    /// Line number, 1-based character column and printable text of the line
    /// holding `offset`, and the offset where the next line starts, as
    /// [`crate::search_index::line_details_bytes_with_cr_policy`] reports
    /// them for the whole file.
    fn line_details(&mut self, offset: usize) -> (u32, u32, String, usize);
    /// The matched bytes, decoded lossily.
    fn match_text(&self, start: usize, end: usize) -> String;
}

/// [`ScanLines`] over a file's whole bytes, building the line table on the
/// first match only.
pub(crate) struct WholeLines<'a> {
    content: &'a [u8],
    line_starts: Option<Vec<usize>>,
    /// The folder-walk grep's rules: every trailing CR is trimmed, and when
    /// the whole file is valid UTF-8 a byte offset inside a character counts
    /// its column from that character's start.
    fallback: bool,
    valid_utf8: Option<Option<&'a str>>,
}

impl<'a> WholeLines<'a> {
    /// Indexed grep's rules: at most one CR is trimmed from the end of a
    /// line's text. That is the CR of a CRLF ending, or, for an unterminated
    /// last line, a bare CR at the very end of the file; a run of CRs keeps
    /// all but its last one.
    pub(crate) fn indexed(content: &'a [u8]) -> Self {
        Self {
            content,
            line_starts: None,
            fallback: false,
            valid_utf8: None,
        }
    }

    /// The folder-walk grep's rules (see the `fallback` field).
    pub(crate) fn fallback(content: &'a [u8]) -> Self {
        Self {
            fallback: true,
            ..Self::indexed(content)
        }
    }
}

impl ScanLines for WholeLines<'_> {
    fn line_details(&mut self, offset: usize) -> (u32, u32, String, usize) {
        let content = self.content;
        let starts = self
            .line_starts
            .get_or_insert_with(|| crate::search_index::line_starts_bytes(content));
        if !self.fallback {
            return crate::search_index::line_details_bytes(content, starts, offset);
        }
        // Byte regexes can start inside a Unicode character. Preserve
        // character columns for valid UTF-8, validating only once after the
        // first match.
        let text = self
            .valid_utf8
            .get_or_insert_with(|| std::str::from_utf8(content).ok());
        let column_offset = text.map_or(offset, |text| text.floor_char_boundary(offset));
        crate::search_index::line_details_bytes_with_cr_policy(content, starts, column_offset, true)
    }

    fn match_text(&self, start: usize, end: usize) -> String {
        String::from_utf8_lossy(&self.content[start..end]).into_owned()
    }
}

/// [`ScanLines`] over a run of whole lines of a file read in steps: the run
/// starts at a line start, and every line in it but the file's last ends
/// with its LF.
pub(crate) struct RegionLines<'a> {
    bytes: &'a [u8],
    /// File offset of `bytes[0]`.
    base: usize,
    /// LFs in the file before `base`.
    lines_before: usize,
    /// LFs in `bytes[..counted]`; matches arrive in file order, so counting
    /// resumes where the last match left off.
    counted: usize,
    counted_lines: usize,
    trim_all_cr: bool,
}

impl RegionLines<'_> {
    /// Line index, line start and end (both in `bytes`), and the file offset
    /// of the next line's start, for the line holding file offset `offset`.
    fn locate(&mut self, offset: usize) -> (usize, usize, usize, usize) {
        let rel = offset - self.base;
        if rel < self.counted {
            self.counted = 0;
            self.counted_lines = 0;
        }
        self.counted_lines += memchr::memchr_iter(b'\n', &self.bytes[self.counted..rel]).count();
        self.counted = rel;
        let line_start = memchr::memrchr(b'\n', &self.bytes[..rel]).map_or(0, |index| index + 1);
        // A line's end is its LF, or the end of the file for an unterminated
        // last line, whose next line "starts" at the end of the file.
        let (line_end, next_line_start) = match memchr::memchr(b'\n', &self.bytes[rel..]) {
            Some(index) => (rel + index, self.base + rel + index + 1),
            None => (self.bytes.len(), self.base + self.bytes.len()),
        };
        (
            self.lines_before + self.counted_lines,
            line_start,
            line_end,
            next_line_start,
        )
    }

    /// The column [`ScanLines::line_details`] reports when the file is
    /// valid UTF-8 under the folder-walk grep's rules: counted from the start
    /// of the character that holds `offset`.
    pub(crate) fn floor_column(&mut self, offset: usize) -> u32 {
        let (_, line_start, _, _) = self.locate(offset);
        let mut floor = offset - self.base;
        while floor > line_start && floor < self.bytes.len() && self.bytes[floor] & 0xC0 == 0x80 {
            floor -= 1;
        }
        column(&self.bytes[line_start..floor])
    }
}

/// 1-based character column after `prefix`, the line's bytes before the
/// match, decoded lossily as the whole-file scan decodes them.
fn column(prefix: &[u8]) -> u32 {
    String::from_utf8_lossy(prefix).chars().count() as u32 + 1
}

impl ScanLines for RegionLines<'_> {
    fn line_details(&mut self, offset: usize) -> (u32, u32, String, usize) {
        let (line_index, line_start, line_end, next_line_start) = self.locate(offset);
        let mut line = &self.bytes[line_start..line_end];
        while line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
            if !self.trim_all_cr {
                break;
            }
        }
        (
            line_index as u32 + 1,
            column(&self.bytes[line_start..offset - self.base]),
            crate::search_index::bounded_grep_line_text_bytes(line),
            next_line_start,
        )
    }

    fn match_text(&self, start: usize, end: usize) -> String {
        String::from_utf8_lossy(&self.bytes[start - self.base..end - self.base]).into_owned()
    }
}

/// A pattern a scan may run over a file read in steps.
pub(crate) enum LineMatcher<'a> {
    /// A non-empty needle with no LF, searched in the ASCII-lowercased bytes
    /// when `case_insensitive_ascii`.
    Literal {
        needle: &'a [u8],
        case_insensitive_ascii: bool,
    },
    /// A regex that passes [`regex_is_line_local`].
    Regex(&'a Regex),
}

/// How a streamed scan ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScanEnd {
    /// It reached the end of the file (or the size at open).
    Finished,
    /// The per-match logic stopped it, or `step_hook` stopped it the way the
    /// per-match logic would have at its next occurrence.
    Stopped,
    /// `step_hook` interrupted it before the end of the file.
    Interrupted(Interrupted),
}

/// Scan the file `source` reads, after `head` (its first bytes, already
/// read), for `matcher`, calling `visit` for each occurrence in file order
/// with the file offsets a whole-buffer scan would give it.
///
/// Only whole lines are searched, so a line-local pattern finds exactly the
/// occurrences a whole-buffer search finds; a line longer than a step is
/// carried until its LF. A zero-length match at the end of a run of lines is
/// the next line's start and is handed over with that line. `step_hook` sees
/// every byte once, in order, before it is searched, and can end the scan
/// with [`ScanEnd::Stopped`] or [`ScanEnd::Interrupted`].
pub(crate) fn scan_lines(
    source: &mut (impl StepSource + ?Sized),
    size_at_open: u64,
    head: Vec<u8>,
    matcher: &LineMatcher<'_>,
    trim_all_cr: bool,
    mut step_hook: impl FnMut(&[u8]) -> Option<ScanEnd>,
    mut visit: impl FnMut(&mut RegionLines<'_>, usize, usize) -> MatchVisit,
) -> io::Result<ScanEnd> {
    let mut steps = Steps::new(source, size_at_open.saturating_sub(head.len() as u64));
    let finder = match matcher {
        LineMatcher::Literal { needle, .. } => Some(memchr::memmem::Finder::new(needle)),
        LineMatcher::Regex(_) => None,
    };
    let mut lowered = Vec::new();
    let mut buffer = head;
    let mut base = 0usize;
    let mut lines_before = 0usize;
    let mut fresh_from = 0usize;
    loop {
        if let Some(end) = step_hook(&buffer[fresh_from..]) {
            return Ok(end);
        }
        if let Some(last_lf) = memchr::memrchr(b'\n', &buffer[fresh_from..]) {
            let end = fresh_from + last_lf + 1;
            let mut lines = RegionLines {
                bytes: &buffer[..end],
                base,
                lines_before,
                counted: 0,
                counted_lines: 0,
                trim_all_cr,
            };
            if !scan_region(
                &mut lines,
                false,
                matcher,
                finder.as_ref(),
                &mut lowered,
                &mut visit,
            ) {
                return Ok(ScanEnd::Stopped);
            }
            lines_before += memchr::memchr_iter(b'\n', &buffer[..end]).count();
            base += end;
            buffer.drain(..end);
        }
        fresh_from = buffer.len();
        if steps.append(&mut buffer)? == 0 {
            break;
        }
    }
    let mut lines = RegionLines {
        bytes: &buffer,
        base,
        lines_before,
        counted: 0,
        counted_lines: 0,
        trim_all_cr,
    };
    if !scan_region(
        &mut lines,
        true,
        matcher,
        finder.as_ref(),
        &mut lowered,
        &mut visit,
    ) {
        return Ok(ScanEnd::Stopped);
    }
    Ok(ScanEnd::Finished)
}

/// Search one run of whole lines; false when `visit` stopped the scan.
fn scan_region(
    lines: &mut RegionLines<'_>,
    last: bool,
    matcher: &LineMatcher<'_>,
    finder: Option<&memchr::memmem::Finder<'_>>,
    lowered: &mut Vec<u8>,
    visit: &mut impl FnMut(&mut RegionLines<'_>, usize, usize) -> MatchVisit,
) -> bool {
    let region = lines.bytes;
    let base = lines.base;
    match (matcher, finder) {
        (
            LineMatcher::Literal {
                needle,
                case_insensitive_ascii,
            },
            Some(finder),
        ) => {
            let haystack: &[u8] = if *case_insensitive_ascii {
                lowered.clear();
                lowered.extend(region.iter().map(u8::to_ascii_lowercase));
                &lowered[..]
            } else {
                region
            };
            let mut start = 0;
            while start <= haystack.len() {
                let Some(position) = finder.find(&haystack[start..]) else {
                    break;
                };
                let offset = start + position;
                if !last && offset == haystack.len() {
                    break;
                }
                start = match visit(lines, base + offset, base + offset + needle.len()) {
                    MatchVisit::Stop => return false,
                    MatchVisit::SameLine => offset + 1,
                    MatchVisit::Recorded { next_line_start } => {
                        (next_line_start - base).max(offset + 1)
                    }
                };
            }
        }
        (LineMatcher::Regex(regex), _) => {
            for matched in regex.find_iter(region) {
                if !last && matched.start() == region.len() {
                    continue;
                }
                if matches!(
                    visit(lines, base + matched.start(), base + matched.end()),
                    MatchVisit::Stop
                ) {
                    return false;
                }
            }
        }
        (LineMatcher::Literal { .. }, None) => {}
    }
    true
}

/// Whether a literal needle may be scanned in steps: it is not empty and
/// holds no LF, so every occurrence lies inside one line.
pub(crate) fn literal_is_line_local(needle: &[u8]) -> bool {
    !needle.is_empty() && !needle.contains(&b'\n')
}

/// Whether every match of `regex` lies inside one line and does not depend on
/// anything outside it, so a scan of whole lines finds the same matches as a
/// scan of the whole file: no look-around or anchor of any kind (that covers
/// `^`, `$`, `\b`, `\A`, `\z` in every mode), no literal or class that can
/// match an LF, and a match language that is valid UTF-8 (the folder-walk
/// grep's columns depend on whole-file validity, and empty matches skip the
/// same positions in both scans).
///
/// The regex's own pattern text is parsed again with the byte regex's
/// settings. The builder's case-insensitivity flag is not recorded on the
/// compiled regex, so the pattern must pass with it both off and on.
pub(crate) fn regex_is_line_local(regex: &Regex) -> bool {
    [false, true].into_iter().all(|case_insensitive| {
        regex_syntax::ParserBuilder::new()
            .utf8(false)
            .case_insensitive(case_insensitive)
            .build()
            .parse(regex.as_str())
            .is_ok_and(|hir| {
                hir.properties().look_set().is_empty()
                    && hir.properties().is_utf8()
                    && excludes_lf(&hir)
            })
    })
}

fn excludes_lf(hir: &regex_syntax::hir::Hir) -> bool {
    use regex_syntax::hir::{Class, HirKind};
    match hir.kind() {
        HirKind::Literal(literal) => !literal.0.contains(&b'\n'),
        HirKind::Class(Class::Bytes(class)) => class
            .iter()
            .all(|range| !(range.start() <= b'\n' && b'\n' <= range.end())),
        HirKind::Class(Class::Unicode(class)) => class
            .iter()
            .all(|range| !(range.start() <= '\n' && '\n' <= range.end())),
        _ => hir.kind().subs().iter().all(excludes_lf),
    }
}

// ---------------------------------------------------------------------------
// Large-read counters for health

/// The readers whose large reads health counts, by site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    /// Indexed grep verification (`search_candidate_file`).
    GrepIndexed,
    /// Indexed grep collection by file (`matching_lines_in_content`).
    GrepCollect,
    /// Folder-walk grep (`fallback_search_file`).
    GrepFallback,
    /// The degraded search fallback's read phase.
    DegradedGrep,
    /// The literal presence check.
    Presence,
    /// `matching_line_from_source`.
    MatchingLine,
    /// The reranker's candidate text.
    CandidateText,
    /// `request_source_file` (definition and reference helpers).
    SourceFile,
    /// `lexical_candidate_exactness`.
    LexicalExactness,
    /// Exact-memo digest reads and the direct digest proof.
    MemoDigest,
    /// The external exact sweep.
    ExternalSweep,
    /// Exact-corpus and anchored verification reads.
    ExactCorpus,
    /// Data-file classification.
    DataFile,
    /// Nearest-name lookups.
    NearestNames,
    /// Grep row line windows.
    LineWindows,
    /// Bounded snippet line reads.
    SnippetLines,
    /// `parser::read_source`.
    ParseSource,
}

const SITES: [(Site, &str); 17] = [
    (Site::GrepIndexed, "grep_indexed"),
    (Site::GrepCollect, "grep_collect"),
    (Site::GrepFallback, "grep_fallback"),
    (Site::DegradedGrep, "degraded_grep"),
    (Site::Presence, "presence"),
    (Site::MatchingLine, "matching_line"),
    (Site::CandidateText, "candidate_text"),
    (Site::SourceFile, "source_file"),
    (Site::LexicalExactness, "lexical_exactness"),
    (Site::MemoDigest, "memo_digest"),
    (Site::ExternalSweep, "external_sweep"),
    (Site::ExactCorpus, "exact_corpus"),
    (Site::DataFile, "data_file"),
    (Site::NearestNames, "nearest_names"),
    (Site::LineWindows, "line_windows"),
    (Site::SnippetLines, "snippet_lines"),
    (Site::ParseSource, "parse_source"),
];

struct Counter {
    count: AtomicU64,
    bytes: AtomicU64,
    largest: AtomicU64,
    whole_count: AtomicU64,
    whole_bytes: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: Counter = Counter {
    count: AtomicU64::new(0),
    bytes: AtomicU64::new(0),
    largest: AtomicU64::new(0),
    whole_count: AtomicU64::new(0),
    whole_bytes: AtomicU64::new(0),
};

static COUNTERS: [Counter; SITES.len()] = [ZERO; SITES.len()];

#[cfg(test)]
thread_local! {
    // Recording tests can exercise the public entry points without racing
    // readers in other tests or resetting their process-wide observations.
    static TEST_COUNTERS: std::cell::RefCell<Option<[Counter; SITES.len()]>> = const { std::cell::RefCell::new(None) };
}

fn with_counter<T>(site: Site, work: impl FnOnce(&Counter) -> T) -> T {
    #[cfg(test)]
    let mut work = Some(work);
    #[cfg(test)]
    if let Some(result) = TEST_COUNTERS.with(|counters| {
        counters
            .borrow()
            .as_ref()
            .map(|counters| work.take().unwrap()(&counters[site as usize]))
    }) {
        return result;
    }
    #[cfg(test)]
    let work = work.unwrap();
    work(&COUNTERS[site as usize])
}

fn record_read(site: Site, size: u64, bytes: u64, whole: bool) {
    if size <= LARGE_READ_BYTES && bytes <= LARGE_READ_BYTES {
        return;
    }
    with_counter(site, |counter| {
        counter.count.fetch_add(1, Ordering::Relaxed);
        counter.bytes.fetch_add(bytes, Ordering::Relaxed);
        counter.largest.fetch_max(bytes, Ordering::Relaxed);
        if whole {
            counter.whole_count.fetch_add(1, Ordering::Relaxed);
            counter.whole_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    });
}

/// Count a streamed read at `site` of a file that was `size` bytes at open, of which
/// `bytes` were read, when the file is a large read. Bytes and the largest
/// read are in bytes read.
pub(crate) fn record(site: Site, size: u64, bytes: u64) {
    record_read(site, size, bytes, false);
}

/// Count a whole read of `bytes` bytes at `site`.
pub(crate) fn record_whole(site: Site, bytes: usize) {
    record_read(site, bytes as u64, bytes as u64, true);
}

struct HealthSite {
    site: &'static str,
    count: u64,
    bytes: u64,
    largest: u64,
    whole_count: u64,
    whole_bytes: u64,
}

fn summarize_health_sites(mut sites: Vec<HealthSite>) -> serde_json::Value {
    sites.retain(|site| site.count > 0);
    let sites_active = sites.len();
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    let mut largest = 0_u64;
    let mut whole_count = 0_u64;
    let mut whole_bytes = 0_u64;
    for site in &sites {
        count = count.saturating_add(site.count);
        bytes = bytes.saturating_add(site.bytes);
        largest = largest.max(site.largest);
        whole_count = whole_count.saturating_add(site.whole_count);
        whole_bytes = whole_bytes.saturating_add(site.whole_bytes);
    }
    sites.sort_unstable_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.site.cmp(right.site))
    });
    // Keep health within SUBC's byte budget while totals still cover every site.
    sites.truncate(4);
    let top = sites
        .into_iter()
        .map(|site| {
            serde_json::json!({
                "site": site.site,
                "count": site.count,
                "bytes": site.bytes,
                "largest": site.largest,
                "whole_count": site.whole_count,
                "whole_bytes": site.whole_bytes,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "count": count,
        "bytes": bytes,
        "largest": largest,
        "whole_count": whole_count,
        "whole_bytes": whole_bytes,
        "sites_active": sites_active,
        "top": top,
    })
}

/// Large reads since start, with totals over all sites and a bounded preview
/// of the four sites that read the most bytes, ordered by bytes then name.
/// Whole-read counters are a subset of the combined count and byte totals.
pub(crate) fn health() -> serde_json::Value {
    summarize_health_sites(
        SITES
            .into_iter()
            .map(|(site, name)| {
                with_counter(site, |counter| HealthSite {
                    site: name,
                    count: counter.count.load(Ordering::Relaxed),
                    bytes: counter.bytes.load(Ordering::Relaxed),
                    largest: counter.largest.load(Ordering::Relaxed),
                    whole_count: counter.whole_count.load(Ordering::Relaxed),
                    whole_bytes: counter.whole_bytes.load(Ordering::Relaxed),
                })
            })
            .collect(),
    )
}

/// Every site's name, for tests that size the health entry at its largest.
#[cfg(test)]
pub(crate) fn site_names() -> impl Iterator<Item = &'static str> {
    SITES.into_iter().map(|(_, name)| name)
}

#[cfg(test)]
pub(crate) fn widest_health_for_test() -> serde_json::Value {
    summarize_health_sites(
        site_names()
            .map(|site| HealthSite {
                site,
                count: u64::MAX,
                bytes: u64::MAX,
                largest: u64::MAX,
                whole_count: u64::MAX,
                whole_bytes: u64::MAX,
            })
            .collect(),
    )
}

/// Count `read`, a loader's whole read of a file, at `site` when it is a
/// large read, and hand it back unchanged.
pub(crate) fn counted<T: AsRef<[u8]>, U>(
    site: Site,
    read: io::Result<(T, U)>,
) -> io::Result<(T, U)> {
    if let Ok((bytes, _)) = &read {
        record_whole(site, bytes.as_ref().len());
    }
    read
}

/// A file's bytes handed out in reads of at most `short` bytes (every read
/// shorter than asked when `short` is small), as a slow or interrupted
/// descriptor would.
#[cfg(test)]
pub(crate) struct SliceSource {
    bytes: Vec<u8>,
    at: usize,
    short: usize,
}

#[cfg(test)]
impl StepSource for SliceSource {
    fn append(&mut self, mut buffer: Vec<u8>, size: usize) -> io::Result<Vec<u8>> {
        let count = size.min(self.short).min(self.bytes.len() - self.at);
        buffer.extend_from_slice(&self.bytes[self.at..self.at + count]);
        self.at += count;
        Ok(buffer)
    }
}

#[cfg(test)]
pub(crate) fn source(bytes: &[u8], short: usize) -> SliceSource {
    SliceSource {
        bytes: bytes.to_vec(),
        at: 0,
        short: short.max(1),
    }
}

/// Generated grep input of at least `min_len` bytes for the scan
/// differential tests: short and empty lines, CRLF and CR-only endings, a
/// run of CRs, lines longer than any small step, multi-byte characters, and
/// (with `invalid`) a stray invalid UTF-8 byte. `last_lf` decides whether
/// the file ends with an LF.
#[cfg(test)]
pub(crate) fn generated_lines(seed: u64, min_len: usize, invalid: bool, last_lf: bool) -> Vec<u8> {
    const PIECES: [&str; 16] = [
        "foo", "FOO", "fooo", "bar1", "x", "é", "Straße", "STRASSE", "日本", "a", "ab", "42", "  ",
        "\t", "zz", "Foo99",
    ];
    let mut state = seed | 1;
    let mut next = |bound: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % bound as u64) as usize
    };
    let mut out = Vec::new();
    while out.len() < min_len {
        let words = match next(10) {
            0 => 0,
            1 => 40 + next(40),
            _ => next(6),
        };
        for _ in 0..words {
            out.extend_from_slice(PIECES[next(PIECES.len())].as_bytes());
            if next(3) == 0 {
                out.push(b' ');
            }
        }
        if invalid && next(25) == 0 {
            out.push([0xFF, 0x80, 0xC3][next(3)]);
        }
        out.extend_from_slice(match next(8) {
            0 => b"\r\n".as_slice(),
            1 => b"\r\r\n",
            2 => b"\r",
            _ => b"\n",
        });
    }
    if !last_lf {
        while out.last() == Some(&b'\n') {
            out.pop();
        }
        out.extend_from_slice("tail foo é".as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random numbers, so a failing case reproduces.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    /// The step sizes every differential test runs: 1 to 8 bytes, a few odd
    /// ones, and a full step.
    const STEPS: [usize; 11] = [1, 2, 3, 4, 5, 6, 7, 8, 13, 64, STEP_BYTES];

    #[test]
    fn site_names_follow_the_site_order() {
        for (index, (site, _)) in SITES.iter().enumerate() {
            assert_eq!(*site as usize, index);
        }
    }

    #[test]
    fn health_without_reads_has_zero_totals_and_no_top_sites() {
        let sites = site_names()
            .map(|site| HealthSite {
                site,
                count: 0,
                bytes: 0,
                largest: 0,
                whole_count: 0,
                whole_bytes: 0,
            })
            .collect();
        assert_eq!(
            summarize_health_sites(sites),
            serde_json::json!({
                "count": 0,
                "bytes": 0,
                "largest": 0,
                "whole_count": 0,
                "whole_bytes": 0,
                "sites_active": 0,
                "top": [],
            })
        );
    }

    #[test]
    fn health_aggregates_all_17_sites_and_orders_four_by_bytes_then_name() {
        let sites = site_names()
            .enumerate()
            .map(|(index, site)| HealthSite {
                site,
                count: index as u64 + 1,
                bytes: (index as u64 % 3 + 1) * 10,
                largest: index as u64 + 1,
                whole_count: index as u64 % 2,
                whole_bytes: (index as u64 % 2) * 10,
            })
            .collect();
        let health = summarize_health_sites(sites);
        assert_eq!(health["sites_active"], 17);
        assert_eq!(health["count"], 153);
        assert_eq!(health["bytes"], 330);
        assert_eq!(health["largest"], 17);
        assert_eq!(health["whole_count"], 8);
        assert_eq!(health["whole_bytes"], 80);
        assert_eq!(
            health["top"],
            serde_json::json!([
                {"site": "exact_corpus", "count": 12, "bytes": 30, "largest": 12, "whole_count": 1, "whole_bytes": 10},
                {"site": "grep_fallback", "count": 3, "bytes": 30, "largest": 3, "whole_count": 0, "whole_bytes": 0},
                {"site": "lexical_exactness", "count": 9, "bytes": 30, "largest": 9, "whole_count": 0, "whole_bytes": 0},
                {"site": "line_windows", "count": 15, "bytes": 30, "largest": 15, "whole_count": 0, "whole_bytes": 0},
            ])
        );
    }

    #[test]
    fn health_totals_saturate_with_all_17_sites_at_maximum() {
        let health = widest_health_for_test();
        assert_eq!(health["sites_active"], 17);
        assert_eq!(health["count"], u64::MAX);
        assert_eq!(health["bytes"], u64::MAX);
        assert_eq!(health["largest"], u64::MAX);
        assert_eq!(health["whole_count"], u64::MAX);
        assert_eq!(health["whole_bytes"], u64::MAX);
        let top = health["top"].as_array().unwrap();
        assert_eq!(top.len(), 4);
        let names = top
            .iter()
            .map(|site| site["site"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "candidate_text",
                "data_file",
                "degraded_grep",
                "exact_corpus"
            ]
        );
        for site in top {
            assert_eq!(site["count"], u64::MAX);
            assert_eq!(site["bytes"], u64::MAX);
            assert_eq!(site["largest"], u64::MAX);
            assert_eq!(site["whole_count"], u64::MAX);
            assert_eq!(site["whole_bytes"], u64::MAX);
        }
    }

    #[test]
    fn whole_and_streamed_reads_stay_at_their_sites() {
        TEST_COUNTERS.with(|counters| *counters.borrow_mut() = Some([ZERO; SITES.len()]));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                TEST_COUNTERS.with(|counters| *counters.borrow_mut() = None);
            }
        }
        let _reset = Reset;
        let size = LARGE_READ_BYTES + 1;
        record_whole(Site::GrepCollect, size as usize);
        // A partial scan is still a large-file read, but not a whole read.
        record(Site::GrepCollect, size, 17);
        record(Site::GrepFallback, size, size);
        record_whole(Site::ParseSource, (size + 1) as usize);
        record_whole(Site::GrepCollect, LARGE_READ_BYTES as usize);
        record(Site::GrepFallback, LARGE_READ_BYTES, LARGE_READ_BYTES);
        let health = health();
        assert_eq!(health["count"], 4);
        assert_eq!(health["bytes"], size * 3 + 18);
        assert_eq!(health["whole_count"], 2);
        assert_eq!(health["whole_bytes"], size * 2 + 1);
        assert_eq!(health["sites_active"], 3);
        let sites = health["top"].as_array().unwrap();
        assert_eq!(
            sites[0],
            serde_json::json!({
                "site": "grep_collect", "count": 2, "bytes": size + 17,
                "largest": size, "whole_count": 1, "whole_bytes": size,
            })
        );
        assert_eq!(sites[1]["site"], "parse_source");
        assert_eq!(sites[1]["whole_count"], 1);
        assert_eq!(sites[1]["whole_bytes"], size + 1);
        assert_eq!(sites[2]["site"], "grep_fallback");
        assert_eq!(sites[2]["whole_count"], 0);
        assert_eq!(sites[2]["whole_bytes"], 0);
    }

    // White_Space characters of every UTF-8 width, and characters that look
    // like spaces but are not White_Space, so they must not split words.
    const SPACES: [&str; 14] = [
        " ", "\t", "\n", "\r", "\u{0B}", "\u{0C}", "\u{85}", "\u{A0}", "\u{1680}", "\u{2007}",
        "\u{2028}", "\u{2029}", "\u{202F}", "\u{3000}",
    ];
    const WORDS: [&str; 10] = [
        "Foo",
        "bar",
        "\u{200B}",
        "\u{FEFF}",
        "Straße",
        "ÉCOLE",
        "日本",
        "x",
        "BaZ",
        "𝔘nicode",
    ];

    fn generated_text(rng: &mut Rng, pieces: usize) -> String {
        let mut text = String::new();
        for _ in 0..pieces {
            if rng.below(2) == 0 {
                for _ in 0..=rng.below(4) {
                    text.push_str(SPACES[rng.below(SPACES.len())]);
                }
            } else {
                text.push_str(WORDS[rng.below(WORDS.len())]);
            }
        }
        text
    }

    #[test]
    fn utf8_check_agrees_with_whole_validation_at_every_split() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            "é".as_bytes().to_vec(),
            "é".as_bytes()[..1].to_vec(),
            "€".as_bytes()[..2].to_vec(),
            vec![0xE2, 0x82, b'a'],
            vec![0xF0, 0x9F, 0x98],
            vec![0xED, 0xA0, 0x80],
            vec![0xC0, 0x80],
            vec![0xFF],
            vec![b'a', 0x80, b'b'],
        ];
        for _ in 0..200 {
            let mut bytes = generated_text(&mut rng, 12).into_bytes();
            if rng.below(3) == 0 && !bytes.is_empty() {
                let at = rng.below(bytes.len());
                bytes[at] = [0x80, 0xFF, 0xC3, 0xE2][rng.below(4)];
            }
            if rng.below(4) == 0 && !bytes.is_empty() {
                bytes.truncate(rng.below(bytes.len()));
            }
            cases.push(bytes);
        }
        for bytes in cases {
            let whole = std::str::from_utf8(&bytes).ok();
            for piece in STEPS {
                let mut check = Utf8Check::default();
                let mut text = String::new();
                let mut valid = true;
                for chunk in bytes.chunks(piece) {
                    if !check.feed(chunk, |part| text.push_str(part)) {
                        valid = false;
                        break;
                    }
                }
                let valid = valid && check.finish();
                assert_eq!(valid, whole.is_some(), "{bytes:?} in pieces of {piece}");
                if let Some(whole) = whole {
                    assert_eq!(text, whole);
                }
            }
        }
    }

    #[test]
    fn phrase_normalizer_matches_normalize_exact_phrase_at_every_split() {
        use crate::commands::semantic_search::exact_lane::normalize_exact_phrase;
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let mut texts: Vec<String> = vec![
            String::new(),
            "   ".into(),
            " lead".into(),
            "trail \u{3000}".into(),
            "a\u{200B}b".into(),
        ];
        texts.extend((0..200).map(|_| generated_text(&mut rng, 16)));
        for text in texts {
            let expected = normalize_exact_phrase(&text);
            let bytes = text.as_bytes();
            for piece in STEPS {
                let mut check = Utf8Check::default();
                let mut normalizer = PhraseNormalizer::default();
                let mut out = Vec::new();
                for chunk in bytes.chunks(piece) {
                    assert!(check.feed(chunk, |part| normalizer.push(part, &mut out)));
                }
                assert!(check.finish());
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    expected,
                    "{text:?} in pieces of {piece}"
                );
            }
        }
    }

    /// Today's whole-read presence answer for `bytes`.
    fn whole_presence(bytes: &[u8], needle: &str) -> PresenceRead {
        use crate::commands::semantic_search::exact_lane::normalize_exact_phrase;
        if crate::search_index::is_binary_bytes(bytes) {
            return PresenceRead::Unreadable;
        }
        match std::str::from_utf8(bytes) {
            Ok(text) => PresenceRead::Holds(normalize_exact_phrase(text).contains(needle)),
            Err(_) => PresenceRead::Unreadable,
        }
    }

    fn assert_presence_matches(bytes: &[u8], phrase: &str) {
        use crate::commands::semantic_search::exact_lane::normalize_exact_phrase;
        let needle = normalize_exact_phrase(phrase);
        let expected = whole_presence(bytes, &needle);
        let mut steps: Vec<usize> = STEPS.to_vec();
        steps.push(needle.len().saturating_sub(1).max(1));
        for step in steps {
            for short in [1, 3, usize::MAX] {
                let (answer, read) = with_test_steps(step, 0, || {
                    let mut source = source(bytes, short);
                    stream_presence(&mut source, bytes.len() as u64, needle.as_bytes(), || None)
                });
                assert_eq!(
                    answer, expected,
                    "{bytes:?} / {needle:?} at step {step}, reads of {short}"
                );
                if matches!(expected, PresenceRead::Holds(_)) {
                    // A found phrase still reads to the end to validate.
                    assert_eq!(read, bytes.len() as u64);
                }
            }
        }
    }

    #[test]
    fn streamed_presence_matches_whole_read_on_listed_cases() {
        let long_space = " ".repeat(40);
        let mut nul_1023 = vec![b'a'; 2000];
        nul_1023[1023] = 0;
        let mut nul_1024 = vec![b'a'; 2000];
        nul_1024[1024] = 0;
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("hello world".encode_utf16().flat_map(u16::to_le_bytes));
        let cases: Vec<(Vec<u8>, &str)> = vec![
            // A phrase before a later invalid byte is still no answer.
            (
                [b"the needle here ".as_slice(), &[0xFF], b" tail"].concat(),
                "needle",
            ),
            // Truncated multi-byte character at the end of the file.
            (
                [b"needle ".as_slice(), &"é".as_bytes()[..1]].concat(),
                "needle",
            ),
            // An incomplete lead byte at a step edge, then a non-continuation.
            (
                [b"ab".as_slice(), &[0xE2, b'c'], b" needle"].concat(),
                "needle",
            ),
            // Whitespace runs longer than a step between phrase words.
            (format!("alpha{long_space}beta").into_bytes(), "alpha beta"),
            (
                format!("alpha{long_space}\t\n beta gamma").into_bytes(),
                "beta gamma",
            ),
            (format!("{long_space}lead word").into_bytes(), "lead word"),
            (long_space.clone().into_bytes(), "x"),
            (long_space.clone().into_bytes(), ""),
            (Vec::new(), ""),
            (Vec::new(), "x"),
            (
                b"needle".to_vec(),
                "needle that is much longer than the file",
            ),
            (b"a".to_vec(), "a"),
            (b"xyz a".to_vec(), "a"),
            (b"any text".to_vec(), ""),
            ([b"bad ".as_slice(), &[0xFF]].concat(), ""),
            // U+000B and U+000C are White_Space; U+200B and U+FEFF are not.
            (b"one\x0Btwo\x0Cthree".to_vec(), "one two three"),
            ("one\u{200B}two".as_bytes().to_vec(), "one two"),
            ("one\u{200B}two".as_bytes().to_vec(), "one\u{200B}two"),
            ("one\u{FEFF}two".as_bytes().to_vec(), "one two"),
            // ASCII case folds across an edge; non-ASCII case does not.
            (b"SoMe MiXeD CaSe".to_vec(), "some mixed case"),
            ("ÉCOLE Straße".as_bytes().to_vec(), "école straße"),
            ("ÉCOLE Straße".as_bytes().to_vec(), "École Straße"),
            // NUL at the last sniffed byte and just after it; a UTF-16 BOM.
            (nul_1023, "aaa"),
            (nul_1024, "aaa"),
            (utf16, "hello"),
        ];
        for (bytes, phrase) in &cases {
            assert_presence_matches(bytes, phrase);
        }
        // Every White_Space character, split at every byte offset by the
        // 1-byte steps above, between two words.
        for space in SPACES {
            let text = format!("left{space}{space}right");
            assert_presence_matches(text.as_bytes(), "left right");
        }
    }

    #[test]
    fn streamed_presence_matches_whole_read_on_generated_files() {
        let mut rng = Rng(0xA076_1D64_78BD_642F);
        for _ in 0..150 {
            let mut bytes = generated_text(&mut rng, 30).into_bytes();
            if rng.below(5) == 0 && !bytes.is_empty() {
                let at = rng.below(bytes.len());
                bytes[at] = 0xFF;
            }
            let phrase = generated_text(&mut rng, 3);
            assert_presence_matches(&bytes, &phrase);
            // A phrase taken from the file itself, so most cases hold it.
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let words: Vec<&str> = text.split_whitespace().collect();
            if words.len() >= 2 {
                let at = rng.below(words.len() - 1);
                assert_presence_matches(&bytes, &words[at..at + 2].join(" "));
            }
        }
    }

    #[test]
    fn streamed_presence_stops_at_the_size_at_open() {
        // A file that grew after it was opened: the bytes past the size at
        // open are not read.
        let bytes = b"start of file, later: needle".to_vec();
        let (answer, read) = with_test_steps(4, 0, || {
            let mut source = source(&bytes, usize::MAX);
            stream_presence(&mut source, 14, b"needle", || None)
        });
        assert_eq!(answer, PresenceRead::Holds(false));
        assert_eq!(read, 14);
        // A file truncated after it was opened ends where its bytes end.
        let (answer, read) = with_test_steps(4, 0, || {
            let mut source = source(b"needle", usize::MAX);
            stream_presence(&mut source, 1 << 20, b"needle", || None)
        });
        assert_eq!(answer, PresenceRead::Holds(true));
        assert_eq!(read, 6);
    }

    #[test]
    fn interrupted_presence_is_never_an_absence() {
        let bytes = b"lots of text without the phrase in it at all".repeat(10);
        for interrupted in [Interrupted::Deadline, Interrupted::Cancelled] {
            for after in [0, 1, 5, 20] {
                let mut checks = 0;
                let (answer, _) = with_test_steps(8, 0, || {
                    let mut source = source(&bytes, usize::MAX);
                    stream_presence(&mut source, bytes.len() as u64, b"needle", || {
                        checks += 1;
                        (checks > after).then_some(interrupted)
                    })
                });
                assert_eq!(answer, PresenceRead::Interrupted(interrupted));
            }
        }
    }

    #[test]
    fn line_local_test_rejects_every_look_and_lf() {
        let local = |pattern: &str| regex_is_line_local(&Regex::new(pattern).unwrap());
        for pattern in [
            "foo",
            "fo+bar",
            "[a-z]+\\d",
            "x*",
            "foo|bar",
            ".",
            "(?i)straße",
            "é",
            "[^a\\n]",
            "\\xFF",
        ] {
            assert!(local(pattern), "{pattern} should be line-local");
        }
        for pattern in [
            "^foo",
            "foo$",
            "(?m)^foo",
            "\\bfoo",
            "\\Bfoo",
            "\\Afoo",
            "foo\\z",
            "a\\nb",
            "[^a]",
            "\\s",
            "\\W",
            "(?s).",
            "[\\x00-\\x7F]",
            "(?-u:\\xFF)",
            "(?-u:.)",
        ] {
            assert!(!local(pattern), "{pattern} must not be line-local");
        }
        assert!(literal_is_line_local(b"abc"));
        assert!(!literal_is_line_local(b""));
        assert!(!literal_is_line_local(b"a\nb"));
    }
}
