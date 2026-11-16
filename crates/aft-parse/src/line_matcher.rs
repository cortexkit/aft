//! Shared line-sequence matching for edit diagnostics and patch application.
//!
//! This module intentionally does not reuse `fuzzy_match`: edit matching works in byte
//! ranges, while apply_patch needs line indexes, EOF anchoring, and unique-only reflow.

use std::collections::{HashMap, HashSet};

/// Avoid spending diagnostic work or memory on files too large to render usefully in an error.
pub const NEAREST_MISS_MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

const NEAREST_MISS_ANCHOR_COUNT: usize = 3;
const NEAREST_MISS_MAX_CANDIDATES: usize = 512;
const NEAREST_MISS_MAX_POSITIONS_PER_ANCHOR: usize = 192;
const NEAREST_MISS_MAX_LINE_COMPARISONS: usize = 100_000;
const NEAREST_MISS_MAX_FUZZY_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NearestMiss {
    /// Zero-based first line of the candidate window.
    pub start: usize,
    /// Zero-based exclusive end of the available candidate window.
    pub end: usize,
    pub matched_lines: usize,
    /// Zero-based wanted-line offset of the first mismatch.
    pub first_divergence: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NearestMissSearch {
    Found(NearestMiss),
    NoSimilarRegion,
    SkippedLargeFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchTier {
    Exact,
    Rstrip,
    Trim,
    Indent,
    Unicode,
    Reflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceMatch {
    pub found: usize,
    pub tier: MatchTier,
    pub line_count: usize,
}

/// Convert smart quotes, dash variants, ellipsis, and NBSP to their ASCII forms; mirrors `patch-parser.ts:207-214`.
pub fn normalize_unicode(input: &str) -> String {
    #[cfg(any(test, feature = "test-support"))]
    NORMALIZED_LINE_ALLOCATIONS.with(|count| count.set(count.get() + 1));
    let mut normalized = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => normalized.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => normalized.push('"'),
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => {
                normalized.push('-');
            }
            '\u{2026}' => normalized.push_str("..."),
            '\u{00A0}' => normalized.push(' '),
            _ => normalized.push(ch),
        }
    }
    normalized
}

/// Replace a leading run of tabs and spaces with the same number of plain spaces; mirrors `patch-parser.ts:227-229`.
pub fn normalize_indent(input: &str) -> String {
    #[cfg(any(test, feature = "test-support"))]
    NORMALIZED_LINE_ALLOCATIONS.with(|count| count.set(count.get() + 1));
    let mut leading_chars = 0;
    let mut leading_bytes = 0;

    for ch in input.chars() {
        if ch != '\t' && ch != ' ' {
            break;
        }
        leading_chars += 1;
        leading_bytes += ch.len_utf8();
    }

    if leading_chars == 0 {
        return input.to_owned();
    }

    let mut normalized = String::with_capacity(input.len());
    normalized.push_str(&" ".repeat(leading_chars));
    normalized.push_str(&input[leading_bytes..]);
    normalized
}

/// Collapse every Unicode whitespace run to one space and trim the ends; mirrors `patch-parser.ts:233-235`.
pub fn normalize_reflow_whitespace(input: &str) -> String {
    #[cfg(any(test, feature = "test-support"))]
    REFLOW_WINDOW_ALLOCATIONS.with(|count| count.set(count.get() + 1));
    #[cfg(any(test, feature = "test-support"))]
    REFLOW_WORK.with(|work| work.set(work.get() + input.len()));
    let mut collapsed = String::with_capacity(input.len());
    let mut in_whitespace = false;

    for ch in input.chars() {
        if ch.is_whitespace() {
            if !in_whitespace {
                collapsed.push(' ');
                in_whitespace = true;
            }
        } else {
            collapsed.push(ch);
            in_whitespace = false;
        }
    }

    collapsed.trim().to_owned()
}

/// Remove every Unicode whitespace character; mirrors `patch-parser.ts:237-239`.
pub fn strip_reflow_whitespace(input: &str) -> String {
    #[cfg(any(test, feature = "test-support"))]
    REFLOW_WINDOW_ALLOCATIONS.with(|count| count.set(count.get() + 1));
    input.chars().filter(|ch| !ch.is_whitespace()).collect()
}

/// Return true when a line has any non-whitespace content; mirrors `patch-parser.ts:241-243`.
pub fn has_reflow_content(input: &str) -> bool {
    input.chars().any(|ch| !ch.is_whitespace())
}

/// Search for a full pattern with a caller-supplied comparator, optionally anchored at EOF; mirrors `patch-parser.ts:247-281`.
///
/// Returns the first line at or after `start_index` where every pattern line
/// compares equal (with `eof`, only the position ending at the last line).
/// `compare` must be an equivalence relation, such as equality of a key
/// derived from each line (the line itself, its trimmed form, ...). With
/// `eof` only the window ending at the last line is checked; otherwise the
/// search is [`normalized_sequence_match`]'s Knuth-Morris-Pratt scan over
/// lines, so it stays linear in file plus pattern lines where trying every
/// start cost file lines × pattern lines on repetitive files.
pub fn try_match<F>(
    lines: &[&str],
    pattern: &[&str],
    start_index: usize,
    compare: F,
    eof: bool,
) -> Option<usize>
where
    F: Fn(&str, &str) -> bool,
{
    normalized_sequence_match(lines, pattern, start_index, eof, compare)
}

/// Add one line-tier comparison to the work counter: one unit plus the
/// shorter line's byte length.
#[cfg(any(test, feature = "test-support"))]
pub fn count_line_comparison(a: &str, b: &str) {
    LINE_MATCH_WORK.with(|work| work.set(work.get() + 1 + a.len().min(b.len())));
}

/// First match of `pattern` at or after `start_index` under `compare`, an
/// equivalence relation on lines. Without `eof` this is a Knuth-Morris-Pratt
/// scan over lines; with `eof` only the window ending at the last line is
/// checked.
fn normalized_sequence_match(
    lines: &[&str],
    pattern: &[&str],
    start_index: usize,
    eof: bool,
    compare: impl Fn(&str, &str) -> bool,
) -> Option<usize> {
    let compare = |line: &str, expected: &str| {
        #[cfg(any(test, feature = "test-support"))]
        count_line_comparison(line, expected);
        compare(line, expected)
    };
    if pattern.is_empty() || pattern.len() > lines.len() {
        return None;
    }
    if eof {
        let start = lines.len() - pattern.len();
        return (start >= start_index
            && lines[start..]
                .iter()
                .zip(pattern)
                .all(|(line, expected)| compare(line, expected)))
        .then_some(start);
    }
    let mut prefix = vec![0; pattern.len()];
    for i in 1..pattern.len() {
        let mut matched = prefix[i - 1];
        loop {
            if compare(pattern[i], pattern[matched]) {
                matched += 1;
                break;
            }
            if matched == 0 {
                break;
            }
            matched = prefix[matched - 1];
        }
        prefix[i] = matched;
    }
    let mut matched = 0;
    for (i, line) in lines.iter().enumerate().skip(start_index) {
        loop {
            if compare(line, pattern[matched]) {
                matched += 1;
                break;
            }
            if matched == 0 {
                break;
            }
            matched = prefix[matched - 1];
        }
        if matched == pattern.len() {
            return Some(i + 1 - pattern.len());
        }
    }
    None
}

fn unicode_characters(input: &str) -> impl Iterator<Item = char> + '_ {
    input.chars().flat_map(|ch| {
        let mapped = match ch {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => '-',
            '\u{00A0}' => ' ',
            _ => ch,
        };
        if ch == '\u{2026}' {
            [Some('.'), Some('.'), Some('.')]
        } else {
            [Some(mapped), None, None]
        }
        .into_iter()
        .flatten()
    })
}

/// Collapse whitespace runs to one space and drop leading and trailing
/// whitespace, lazily. Only the reference scan in the tests still checks
/// windows this way.
#[cfg(any(test, feature = "test-support"))]
fn collapsed_whitespace(chars: impl Iterator<Item = char>) -> impl Iterator<Item = char> {
    let mut started = false;
    let mut space = false;
    chars.flat_map(move |ch| {
        let emitted = if ch.is_whitespace() {
            space = started;
            [None, None]
        } else {
            let emitted = if space {
                [Some(' '), Some(ch)]
            } else {
                [Some(ch), None]
            };
            space = false;
            started = true;
            emitted
        };
        emitted.into_iter().flatten()
    })
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static NORMALIZED_LINE_ALLOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REFLOW_WINDOW_ALLOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Candidate windows the reflow search verified with its whitespace check.
    static REFLOW_STARTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Byte steps of the reflow search's substring scan, so tests can check
    /// that it stays linear in the file and pattern sizes.
    pub static REFLOW_SCAN_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Work done by the reflow pass: each file character read to build its
    /// stream, each step building the needle's failure table and scanning the
    /// stream, and each byte normalized. Tests bound this by the file and
    /// needle sizes for the whole pass.
    pub static REFLOW_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Work done by the line-by-line tiers: one unit per line comparison
    /// plus the shorter line's byte length. Tests bound this counter by file
    /// plus needle size.
    pub static LINE_MATCH_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "test-support"))]
fn count_scan_step() {
    REFLOW_SCAN_STEPS.with(|steps| steps.set(steps.get() + 1));
    REFLOW_WORK.with(|work| work.set(work.get() + 1));
}

/// Line windows whose text equals a needle's text once whitespace is
/// normalized (every whitespace run, line breaks included, collapsed to one
/// space and the ends trimmed), in ascending order of their first line.
///
/// The file is normalized once into a stream: every whitespace run between
/// two non-whitespace characters becomes one space, and each line with
/// content records where its first character starts and its last character
/// ends in that stream. A window from line `s` to line `e` normalizes to
/// exactly the stream bytes from line `s`'s start to line `e - 1`'s end:
/// normalization trims the window's leading and trailing whitespace, and
/// every remaining whitespace run lies inside that span. So
/// the matching windows are exactly the occurrences of the needle's
/// normalized bytes that start where a line with content starts and end
/// where a line with content ends. Knuth-Morris-Pratt finds every
/// occurrence, overlapping ones included, and the line-boundary check walks
/// two forward-only cursors, so the whole search, spacing included, is
/// linear in the file and needle sizes and never rescans a window.
/// Extending a window from every line instead cost file lines × needle
/// lines, and checking each whitespace-stripped hit's spacing afterwards
/// cost candidates × needle size on repetitive files.
pub struct ReflowCandidates<'a> {
    stream: Vec<u8>,
    needle: &'a [u8],
    failure: Vec<usize>,
    /// `(stream offset, line index)` where each line with content starts and
    /// ends; both offsets strictly increase with the line index.
    starts: Vec<(usize, usize)>,
    ends: Vec<(usize, usize)>,
    position: usize,
    matched: usize,
    /// First entries of `starts` and `ends` not yet passed. Occurrences are
    /// found in increasing offset order, so these only move forward.
    start_cursor: usize,
    end_cursor: usize,
}

impl<'a> ReflowCandidates<'a> {
    /// Matching windows among `lines`, numbered from 0 like `lines` itself.
    /// `needle_normalized` must already be normalized as described on the
    /// type (see [`normalize_reflow_whitespace`]) and must not be empty.
    pub fn new(lines: &[&str], needle_normalized: &'a str) -> Self {
        let needle = needle_normalized.as_bytes();
        // Sized up front so building the stream allocates a fixed number of
        // times, never once per line.
        let mut stream = Vec::with_capacity(lines.iter().map(|line| line.len() + 1).sum::<usize>());
        let mut starts = Vec::with_capacity(lines.len());
        let mut ends = Vec::with_capacity(lines.len());
        // A space is owed once whitespace follows emitted text; it is written
        // only when more text follows, so the stream has no leading or
        // trailing space and never two in a row.
        let mut owe_space = false;
        for (index, line) in lines.iter().enumerate() {
            let mut first = None;
            let mut last = 0;
            for ch in line.chars() {
                #[cfg(any(test, feature = "test-support"))]
                REFLOW_WORK.with(|work| work.set(work.get() + 1));
                if ch.is_whitespace() {
                    owe_space = !stream.is_empty();
                    continue;
                }
                if owe_space {
                    stream.push(b' ');
                    owe_space = false;
                }
                first.get_or_insert(stream.len());
                let mut buffer = [0u8; 4];
                stream.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                last = stream.len();
            }
            if let Some(first) = first {
                starts.push((first, index));
                ends.push((last, index));
            }
            // The break between this line and the next is whitespace too.
            owe_space = !stream.is_empty();
        }
        // failure[i]: length of the longest proper prefix of needle[..=i]
        // that is also its suffix.
        let mut failure = vec![0; needle.len()];
        let mut length = 0;
        for index in 1..needle.len() {
            #[cfg(any(test, feature = "test-support"))]
            REFLOW_WORK.with(|work| work.set(work.get() + 1));
            while length > 0 && needle[index] != needle[length] {
                #[cfg(any(test, feature = "test-support"))]
                REFLOW_WORK.with(|work| work.set(work.get() + 1));
                length = failure[length - 1];
            }
            if needle[index] == needle[length] {
                length += 1;
            }
            failure[index] = length;
        }
        Self {
            stream,
            needle,
            failure,
            starts,
            ends,
            position: 0,
            matched: 0,
            start_cursor: 0,
            end_cursor: 0,
        }
    }
}

impl Iterator for ReflowCandidates<'_> {
    /// `(first line, end line exclusive)`.
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.needle.is_empty() {
            return None;
        }
        while self.position < self.stream.len() {
            let byte = self.stream[self.position];
            self.position += 1;
            while self.matched > 0 && self.needle[self.matched] != byte {
                #[cfg(any(test, feature = "test-support"))]
                count_scan_step();
                self.matched = self.failure[self.matched - 1];
            }
            #[cfg(any(test, feature = "test-support"))]
            count_scan_step();
            if self.needle[self.matched] == byte {
                self.matched += 1;
            }
            if self.matched < self.needle.len() {
                continue;
            }
            self.matched = self.failure[self.matched - 1];
            let end = self.position;
            let start = end - self.needle.len();
            // Keep only occurrences that start at the start of a line with
            // content and end at the end of a line with content.
            while self
                .starts
                .get(self.start_cursor)
                .is_some_and(|&(offset, _)| offset < start)
            {
                self.start_cursor += 1;
            }
            while self
                .ends
                .get(self.end_cursor)
                .is_some_and(|&(offset, _)| offset < end)
            {
                self.end_cursor += 1;
            }
            let (Some(&(first_offset, first)), Some(&(last_offset, last))) = (
                self.starts.get(self.start_cursor),
                self.ends.get(self.end_cursor),
            ) else {
                continue;
            };
            if first_offset != start || last_offset != end {
                continue;
            }
            return Some((first, last + 1));
        }
        None
    }
}

/// Find one unique whitespace-reflowed window, returning `(found_line, line_count)`; mirrors `patch-parser.ts:310-351`.
pub fn find_reflow_match(
    lines: &[&str],
    pattern: &[&str],
    start_index: usize,
) -> Option<(usize, usize)> {
    let normalized_needle = normalize_reflow_whitespace(&pattern.join("\n"));
    if normalized_needle.is_empty() {
        return None;
    }
    let searched = lines.get(start_index..).unwrap_or_default();
    let mut first_match = None;
    for (start, end) in ReflowCandidates::new(searched, &normalized_needle) {
        #[cfg(any(test, feature = "test-support"))]
        REFLOW_STARTS.with(|count| count.set(count.get() + 1));
        if first_match
            .replace((start_index + start, end - start))
            .is_some()
        {
            return None;
        }
    }
    first_match
}

/// Keep exact first-hit matching, but refuse ambiguous non-exact fallbacks.
pub fn seek_sequence_tiered(
    lines: &[&str],
    pattern: &[&str],
    start_index: usize,
    eof: bool,
) -> Option<SequenceMatch> {
    seek_sequence_tiered_checked(lines, pattern, start_index, eof)
        .ok()
        .flatten()
}

fn unique_match(
    first: Option<usize>,
    next: impl FnOnce(usize) -> Option<usize>,
) -> Result<Option<usize>, &'static str> {
    if let Some(first) = first {
        if next(first + 1).is_some() {
            return Err("Ambiguous non-exact match");
        }
    }
    Ok(first)
}

pub fn seek_sequence_tiered_checked(
    lines: &[&str],
    pattern: &[&str],
    start_index: usize,
    eof: bool,
) -> Result<Option<SequenceMatch>, &'static str> {
    if pattern.is_empty() {
        return Ok(None);
    }
    let matched = |found, tier| {
        Some(SequenceMatch {
            found,
            tier,
            line_count: pattern.len(),
        })
    };
    if let Some(found) = try_match(lines, pattern, start_index, |a, b| a == b, eof) {
        return Ok(matched(found, MatchTier::Exact));
    }
    for (tier, compare) in [
        (
            MatchTier::Rstrip,
            (|a: &str, b: &str| a.trim_end() == b.trim_end()) as fn(&str, &str) -> bool,
        ),
        (
            MatchTier::Trim,
            (|a: &str, b: &str| a.trim() == b.trim()) as fn(&str, &str) -> bool,
        ),
    ] {
        let found = unique_match(
            try_match(lines, pattern, start_index, compare, eof),
            |next| try_match(lines, pattern, next, compare, eof),
        )?;
        if let Some(found) = found {
            return Ok(matched(found, tier));
        }
    }
    let indent = |a: &str, b: &str| a.trim() == b.trim();
    let found = unique_match(
        normalized_sequence_match(lines, pattern, start_index, eof, indent),
        |next| normalized_sequence_match(lines, pattern, next, eof, indent),
    )?;
    if let Some(found) = found {
        return Ok(matched(found, MatchTier::Indent));
    }
    let unicode = |a: &str, b: &str| unicode_characters(a.trim()).eq(unicode_characters(b.trim()));
    let found = unique_match(
        normalized_sequence_match(lines, pattern, start_index, eof, unicode),
        |next| normalized_sequence_match(lines, pattern, next, eof, unicode),
    )?;
    if let Some(found) = found {
        return Ok(matched(found, MatchTier::Unicode));
    }
    if eof {
        return Ok(None);
    }
    Ok(
        find_reflow_match(lines, pattern, start_index).map(|(found, line_count)| SequenceMatch {
            found,
            tier: MatchTier::Reflow,
            line_count,
        }),
    )
}

fn add_sampled_candidates(
    candidates: &mut HashSet<usize>,
    positions: &[usize],
    wanted_offset: usize,
    candidate_limit: usize,
) {
    let remaining = candidate_limit.saturating_sub(candidates.len());
    let sample_count = positions
        .len()
        .min(NEAREST_MISS_MAX_POSITIONS_PER_ANCHOR)
        .min(remaining);
    if sample_count == 0 {
        return;
    }

    for sample in 0..sample_count {
        let position_index = if sample_count == 1 {
            0
        } else {
            sample * (positions.len() - 1) / (sample_count - 1)
        };
        let file_position = positions[position_index];
        if let Some(start) = file_position.checked_sub(wanted_offset) {
            candidates.insert(start);
        }
    }
}

fn score_nearest_miss(lines: &[&str], pattern: &[&str], start: usize) -> NearestMiss {
    let end = (start + pattern.len()).min(lines.len());
    let matched_lines = pattern
        .iter()
        .enumerate()
        .filter(|(offset, expected)| {
            lines
                .get(start + offset)
                .is_some_and(|actual| actual.trim() == expected.trim())
        })
        .count();
    let first_divergence = pattern
        .iter()
        .enumerate()
        .find(|(offset, expected)| {
            lines
                .get(start + offset)
                .is_none_or(|actual| actual.trim() != expected.trim())
        })
        .map_or(pattern.len(), |(offset, _)| offset);

    NearestMiss {
        start,
        end,
        matched_lines,
        first_divergence,
    }
}

fn is_better_nearest_miss(candidate: NearestMiss, current: NearestMiss) -> bool {
    candidate.matched_lines > current.matched_lines
        || (candidate.matched_lines == current.matched_lines
            && (candidate.first_divergence > current.first_divergence
                || (candidate.first_divergence == current.first_divergence
                    && candidate.start < current.start)))
}

fn best_scored_candidate(
    lines: &[&str],
    pattern: &[&str],
    candidates: HashSet<usize>,
) -> Option<NearestMiss> {
    candidates
        .into_iter()
        .filter(|start| *start < lines.len())
        .map(|start| score_nearest_miss(lines, pattern, start))
        .fold(None, |best, candidate| match best {
            Some(current) if !is_better_nearest_miss(candidate, current) => Some(current),
            _ => Some(candidate),
        })
}

fn normalize_fuzzy_line(line: &str) -> String {
    normalize_unicode(&normalize_reflow_whitespace(line))
}

fn normalized_prefix_score(normalized_wanted: &str, actual: &str) -> Option<usize> {
    let actual = normalize_fuzzy_line(actual);
    let wanted_len = normalized_wanted
        .chars()
        .take(NEAREST_MISS_MAX_FUZZY_CHARS)
        .count();
    if wanted_len < 4 {
        return None;
    }

    let common = normalized_wanted
        .chars()
        .zip(actual.chars())
        .take(NEAREST_MISS_MAX_FUZZY_CHARS)
        .take_while(|(wanted_char, actual_char)| wanted_char == actual_char)
        .count();
    (common >= 4 && common * 2 >= wanted_len).then_some(common)
}

fn rarest_wanted_line<'a>(pattern: &'a [&'a str]) -> Option<(usize, &'a str)> {
    let mut frequencies: HashMap<&str, usize> = HashMap::new();
    for line in pattern
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
    {
        *frequencies.entry(line).or_default() += 1;
    }

    pattern
        .iter()
        .enumerate()
        .map(|(offset, line)| (offset, line.trim()))
        .filter(|(_, line)| !line.is_empty())
        .min_by_key(|(offset, line)| {
            (
                frequencies.get(line).copied().unwrap_or(usize::MAX),
                std::cmp::Reverse(line.chars().count()),
                *offset,
            )
        })
}

/// Find a bounded best-effort candidate after every accepted match tier has failed.
pub fn find_nearest_miss(
    lines: &[&str],
    pattern: &[&str],
    file_size_bytes: usize,
) -> NearestMissSearch {
    if file_size_bytes > NEAREST_MISS_MAX_FILE_BYTES {
        return NearestMissSearch::SkippedLargeFile;
    }
    if lines.is_empty() || pattern.is_empty() {
        return NearestMissSearch::NoSimilarRegion;
    }

    let mut line_index: HashMap<&str, Vec<usize>> = HashMap::new();
    for (position, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            line_index.entry(trimmed).or_default().push(position);
        }
    }

    let candidate_limit = NEAREST_MISS_MAX_CANDIDATES
        .min((NEAREST_MISS_MAX_LINE_COMPARISONS / pattern.len().max(1)).max(1));
    let mut anchor_positions: Vec<(usize, &[usize])> = pattern
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(NEAREST_MISS_ANCHOR_COUNT)
        .filter_map(|(offset, line)| {
            line_index
                .get(line.trim())
                .map(|positions| (offset, positions.as_slice()))
        })
        .collect();
    anchor_positions.sort_by_key(|(offset, positions)| (positions.len(), *offset));

    let mut candidates = HashSet::new();
    for (wanted_offset, positions) in anchor_positions {
        add_sampled_candidates(&mut candidates, positions, wanted_offset, candidate_limit);
        if candidates.len() >= candidate_limit {
            break;
        }
    }
    if let Some(best) = best_scored_candidate(lines, pattern, candidates) {
        return NearestMissSearch::Found(best);
    }

    let Some((wanted_offset, wanted_line)) = rarest_wanted_line(pattern) else {
        return NearestMissSearch::NoSimilarRegion;
    };
    let normalized_wanted = normalize_fuzzy_line(wanted_line);
    let mut best_prefix = 0;
    let mut fuzzy_candidates = HashSet::new();
    for (file_position, actual_line) in lines.iter().enumerate() {
        let Some(start) = file_position.checked_sub(wanted_offset) else {
            continue;
        };
        let Some(prefix_score) = normalized_prefix_score(&normalized_wanted, actual_line) else {
            continue;
        };
        if prefix_score > best_prefix {
            best_prefix = prefix_score;
            fuzzy_candidates.clear();
        }
        if prefix_score == best_prefix && fuzzy_candidates.len() < candidate_limit {
            fuzzy_candidates.insert(start);
        }
    }

    best_scored_candidate(lines, pattern, fuzzy_candidates)
        .map_or(NearestMissSearch::NoSimilarRegion, NearestMissSearch::Found)
}

#[cfg(test)]
mod tests {
    #[test]
    fn tiered_match_choice_matches_sliding_reference_on_mixed_text() {
        use super::*;
        let cases = [
            (
                vec!["  héllo", "\tworld", "target"],
                vec![" héllo", " world"],
                false,
            ),
            (vec!["before", "‘quoted’", "after"], vec!["'quoted'"], false),
            (vec!["a  b", " c", "last"], vec!["a b c"], false),
            (vec!["x\r", "  y\r", "z"], vec!["x", "y"], false),
            (vec!["x", " \ty"], vec!["y"], true),
        ];
        for (lines, pattern, eof) in cases {
            let expected = [
                (
                    MatchTier::Exact,
                    try_match(&lines, &pattern, 0, |a, b| a == b, eof),
                ),
                (
                    MatchTier::Rstrip,
                    try_match(
                        &lines,
                        &pattern,
                        0,
                        |a, b| a.trim_end() == b.trim_end(),
                        eof,
                    ),
                ),
                (
                    MatchTier::Trim,
                    try_match(&lines, &pattern, 0, |a, b| a.trim() == b.trim(), eof),
                ),
                (
                    MatchTier::Indent,
                    try_match(
                        &lines,
                        &pattern,
                        0,
                        |a, b| normalize_indent(a).trim_end() == normalize_indent(b).trim_end(),
                        eof,
                    ),
                ),
                (
                    MatchTier::Unicode,
                    try_match(
                        &lines,
                        &pattern,
                        0,
                        |a, b| normalize_unicode(a.trim()) == normalize_unicode(b.trim()),
                        eof,
                    ),
                ),
            ]
            .into_iter()
            .find_map(|(tier, found)| {
                found.map(|found| SequenceMatch {
                    found,
                    tier,
                    line_count: pattern.len(),
                })
            })
            .or_else(|| {
                (!eof)
                    .then(|| find_reflow_match(&lines, &pattern, 0))
                    .flatten()
                    .map(|(found, line_count)| SequenceMatch {
                        found,
                        tier: MatchTier::Reflow,
                        line_count,
                    })
            });
            assert_eq!(seek_sequence_tiered(&lines, &pattern, 0, eof), expected);
        }
    }

    #[test]
    fn ambiguous_reflow_stops_after_second_window() {
        let lines = vec!["x y"; 5_000];
        super::REFLOW_STARTS.with(|count| count.set(0));
        assert_eq!(super::find_reflow_match(&lines, &["x y"], 0), None);
        let visited = super::REFLOW_STARTS.with(|count| count.get());
        assert_eq!(visited, 2);
        eprintln!(
            "ambiguous reflow starts: baseline {}, early exit {visited}",
            lines.len()
        );
        assert_eq!(
            super::find_reflow_match(&[" ", "a b", "  "], &["a b"], 0),
            Some((1, 1))
        );
        assert_eq!(
            super::find_reflow_match(&["one", "two"], &["absent"], 0),
            None
        );
    }

    #[test]
    fn normalized_search_is_linear_on_repeated_prefix_and_preserves_first_match() {
        use std::cell::Cell;
        let lines = vec!["same"; 5_000];
        let mut pattern = vec!["same"; 60];
        pattern[59] = "missing";
        let calls = Cell::new(0);
        let found = super::normalized_sequence_match(&lines, &pattern, 0, false, |a, b| {
            calls.set(calls.get() + 1);
            a == b
        });
        assert_eq!(found, None);
        assert!(calls.get() <= 2 * (lines.len() + pattern.len()));
        // Baseline: try every start in turn, as `try_match` once did.
        let baseline_calls = Cell::new(0);
        let baseline = (0..=lines.len() - pattern.len()).find(|&start| {
            pattern.iter().enumerate().all(|(offset, expected)| {
                baseline_calls.set(baseline_calls.get() + 1);
                lines[start + offset] == *expected
            })
        });
        assert_eq!(found, baseline);
        assert!(baseline_calls.get() > calls.get() * 20);
        eprintln!(
            "repeated-prefix comparisons: baseline {}, indexed {}",
            baseline_calls.get(),
            calls.get()
        );
        let mut found_lines = lines.clone();
        found_lines[4_999] = "missing";
        assert_eq!(
            super::normalized_sequence_match(&found_lines, &pattern, 0, false, |a, b| a == b),
            Some(4_940)
        );
        assert_eq!(
            super::find_reflow_match(&["hello", "hello", "hello"], &["hello"], 0),
            None
        );
    }
    use super::*;

    fn assert_match(
        actual: Option<SequenceMatch>,
        found: usize,
        tier: MatchTier,
        line_count: usize,
    ) {
        assert_eq!(
            actual,
            Some(SequenceMatch {
                found,
                tier,
                line_count,
            })
        );
    }

    #[test]
    fn normalization_helpers_match_patch_parser_sources() {
        assert_eq!(
            normalize_unicode("‘’‚‛“”„‟‐‑‒–—―…\u{00A0}"),
            "''''\"\"\"\"------... "
        );
        assert_eq!(normalize_indent("\t  alpha\t beta  "), "   alpha\t beta  ");
        assert_eq!(normalize_indent(""), "");
        assert_eq!(
            normalize_reflow_whitespace(" \talpha\n\u{00A0} beta  "),
            "alpha beta"
        );
        assert_eq!(
            strip_reflow_whitespace(" \talpha\n\u{00A0} beta  "),
            "alphabeta"
        );
        assert!(has_reflow_content("\u{00A0}x"));
        assert!(!has_reflow_content(" \t\n"));
    }

    #[test]
    fn exact_tier_wins_without_upgrading_to_later_tiers() {
        assert_match(
            seek_sequence_tiered(&["alpha", "beta"], &["beta"], 0, false),
            1,
            MatchTier::Exact,
            1,
        );
    }

    #[test]
    fn rstrip_tier_wins_before_trim() {
        assert_match(
            seek_sequence_tiered(&["alpha   "], &["alpha"], 0, false),
            0,
            MatchTier::Rstrip,
            1,
        );
    }

    #[test]
    fn trim_tier_wins_before_indent_and_unicode() {
        assert_match(
            seek_sequence_tiered(&["  alpha  "], &["alpha"], 0, false),
            0,
            MatchTier::Trim,
            1,
        );
    }

    #[test]
    fn indent_normalization_matches_tab_space_drift_but_trim_shadows_the_tier() {
        assert_eq!(normalize_indent("\treturn 42;"), " return 42;");
        assert_eq!(normalize_indent(" return 42;"), " return 42;");
        assert_eq!(
            try_match(
                &["\treturn 42;"],
                &[" return 42;"],
                0,
                |a, b| normalize_indent(a).trim_end() == normalize_indent(b).trim_end(),
                false,
            ),
            Some(0)
        );
        // Expect Trim, not Indent, for tab-vs-space input: a leading tab-vs-space drift
        // is already accepted by the earlier trim tier, so the nominal indent tier is shadowed.
        assert_match(
            seek_sequence_tiered(&["\treturn 42;"], &["    return 42;"], 0, false),
            0,
            MatchTier::Trim,
            1,
        );
    }

    #[test]
    fn unicode_tier_normalizes_smart_punctuation_after_stricter_tiers_fail() {
        assert_match(
            seek_sequence_tiered(
                &["const label = “alpha”—beta…;"],
                &["const label = \"alpha\"-beta...;"],
                0,
                false,
            ),
            0,
            MatchTier::Unicode,
            1,
        );
    }

    #[test]
    fn reflow_tier_matches_one_line_hunk_against_three_line_formatter_split() {
        let lines = [
            "function demo() {",
            "  const value = alpha +",
            "    beta +",
            "    gamma;",
            "  return value;",
            "}",
        ];
        let pattern = ["  const value = alpha + beta + gamma;"];

        assert_match(
            seek_sequence_tiered(&lines, &pattern, 0, false),
            1,
            MatchTier::Reflow,
            3,
        );
    }

    #[test]
    fn rejects_ambiguous_reflow_matches_instead_of_choosing_a_window() {
        // Reject a reflow match when the pattern could match more than one distinct window.
        let lines = [
            "const value = alpha +",
            "  beta +",
            "  gamma;",
            "",
            "const value = alpha +",
            "  beta +",
            "  gamma;",
        ];
        let pattern = ["const value = alpha + beta + gamma;"];

        assert_eq!(find_reflow_match(&lines, &pattern, 0), None);
        assert_eq!(seek_sequence_tiered(&lines, &pattern, 0, false), None);
    }

    #[test]
    fn uses_line_contiguous_match_before_considering_reflow_candidate() {
        // A line-contiguous match wins before any reflow candidate is considered.
        let lines = [
            "const value = alpha +",
            "  beta +",
            "  gamma;",
            "const value = alpha + beta + gamma;",
        ];
        let pattern = ["const value = alpha + beta + gamma;"];

        assert_match(
            seek_sequence_tiered(&lines, &pattern, 0, false),
            3,
            MatchTier::Exact,
            1,
        );
    }

    #[test]
    fn eof_hunk_only_matches_the_tail_and_never_forward_scans() {
        // EOF-anchored hunks match only the tail and never forward-scan.
        let pattern = ["marker", "old"];

        assert_match(
            seek_sequence_tiered(
                &["header", "marker", "old", "middle", "marker", "old"],
                &pattern,
                0,
                true,
            ),
            4,
            MatchTier::Exact,
            2,
        );
        assert_eq!(
            seek_sequence_tiered(
                &["header", "marker", "old", "middle", "marker", "changed"],
                &pattern,
                0,
                true,
            ),
            None
        );
    }

    #[test]
    fn eof_hunk_skips_reflow_even_when_the_tail_would_reflow_match() {
        let lines = ["header", "const value = alpha +", "  beta +", "  gamma;"];
        let pattern = ["const value = alpha + beta + gamma;"];

        assert_eq!(find_reflow_match(&lines, &pattern, 0), Some((1, 3)));
        assert_eq!(seek_sequence_tiered(&lines, &pattern, 0, true), None);
    }

    #[test]
    fn try_match_honors_start_index_for_forward_scans_and_eof_anchor() {
        assert_eq!(
            try_match(&["a", "b", "a", "b"], &["a", "b"], 1, |a, b| a == b, false),
            Some(2)
        );
        assert_eq!(
            try_match(&["a", "b", "a", "b"], &["a", "b"], 3, |a, b| a == b, false),
            None
        );
        assert_eq!(
            try_match(&["a", "b", "a", "b"], &["a", "b"], 3, |a, b| a == b, true),
            None
        );
    }

    #[test]
    fn nearest_miss_scores_matching_lines_across_the_candidate_window() {
        let lines = [
            "header",
            "  const first = 1;",
            "  const actual = 2;",
            "  return first;",
            "separator",
            "  const first = 1;",
            "  unrelated",
            "  unrelated",
        ];
        let pattern = [
            "  const first = 1;",
            "  const expected = 2;",
            "  return first;",
        ];

        assert_eq!(
            find_nearest_miss(&lines, &pattern, 128),
            NearestMissSearch::Found(NearestMiss {
                start: 1,
                end: 4,
                matched_lines: 2,
                first_divergence: 1,
            })
        );
    }

    #[test]
    fn nearest_miss_uses_a_strong_prefix_when_no_anchor_matches_exactly() {
        assert_eq!(
            find_nearest_miss(
                &["header", "const expected_value = 2;", "footer"],
                &["const expected_value = 1;"],
                42,
            ),
            NearestMissSearch::Found(NearestMiss {
                start: 1,
                end: 2,
                matched_lines: 0,
                first_divergence: 0,
            })
        );
    }

    #[test]
    fn nearest_miss_reports_no_region_when_anchors_and_prefixes_are_absent() {
        assert_eq!(
            find_nearest_miss(
                &["alpha", "beta", "gamma"],
                &["completely unrelated line"],
                17,
            ),
            NearestMissSearch::NoSimilarRegion
        );
    }

    #[test]
    fn nearest_miss_skips_files_larger_than_the_diagnostic_limit() {
        let synthetic_file = "x".repeat(NEAREST_MISS_MAX_FILE_BYTES + 1);
        assert_eq!(
            find_nearest_miss(
                &[synthetic_file.as_str()],
                &["wanted content"],
                synthetic_file.len(),
            ),
            NearestMissSearch::SkippedLargeFile
        );
    }
}

#[cfg(test)]
mod audit11_regressions {
    use super::*;

    #[test]
    fn fallback_matching_does_not_allocate_normalized_file_lines() {
        NORMALIZED_LINE_ALLOCATIONS.with(|count| count.set(0));
        let result = seek_sequence_tiered(&["one – two"], &["one - two"], 0, false).unwrap();
        assert_eq!(result.tier, MatchTier::Unicode);
        assert_eq!(NORMALIZED_LINE_ALLOCATIONS.with(|count| count.get()), 0);
    }

    #[test]
    fn reflow_matching_does_not_allocate_candidate_windows() {
        REFLOW_WINDOW_ALLOCATIONS.with(|count| count.set(0));
        assert_eq!(
            find_reflow_match(&["x", "a", "b", "y", "a  b"], &["a b"], 0),
            None
        );
        assert_eq!(
            find_reflow_match(&["x", "a", "b"], &["a b"], 0),
            Some((1, 2))
        );
        // Only the needle is normalized, once per search; file lines are
        // streamed in place and candidate windows are never materialized.
        assert_eq!(REFLOW_WINDOW_ALLOCATIONS.with(|count| count.get()), 2);
    }
}

/// Shared by the reflow differential tests here and in `fuzzy_match`: a
/// deterministic input generator and a brute-force candidate list.
#[cfg(any(test, feature = "test-support"))]
pub mod reflow_test_support {
    /// xorshift64*: deterministic, so a failing case can be replayed.
    pub struct Rng(u64);

    impl Rng {
        pub fn new(seed: u64) -> Self {
            Self(seed.max(1))
        }

        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        pub fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound.max(1) as u64) as usize
        }
    }

    /// Line pieces mixing words, spacing variants, whitespace-only and empty
    /// lines, tabs, a non-ASCII letter and an ideographic space.
    const TOKENS: &[&str] = &[
        "a",
        "b",
        "ab",
        "ba",
        "a b",
        "a  b",
        "b a",
        "\ta",
        "b\t",
        "",
        "  ",
        "\t",
        "é",
        "é a",
        "x",
        "xy",
        "a\u{3000}b",
    ];

    /// File lines: random pieces, sometimes with a repeated block so that
    /// candidates overlap and repeat.
    pub fn file_lines(rng: &mut Rng) -> Vec<String> {
        let mut lines = Vec::new();
        let count = 1 + rng.below(13);
        while lines.len() < count {
            if rng.below(4) == 0 {
                let block = (0..1 + rng.below(3))
                    .map(|_| TOKENS[rng.below(TOKENS.len())].to_string())
                    .collect::<Vec<_>>();
                for _ in 0..2 + rng.below(3) {
                    lines.extend(block.iter().cloned());
                }
            } else {
                lines.push(TOKENS[rng.below(TOKENS.len())].to_string());
            }
        }
        lines.truncate(count);
        lines
    }

    /// Rewrite every whitespace run, as a reflow would, sometimes removing it
    /// (non-whitespace text unchanged, spacing different) and sometimes
    /// adding leading or trailing whitespace.
    fn reflow(rng: &mut Rng, text: &str) -> String {
        const RUNS: &[&str] = &[" ", "\n", "  ", "\t", " \n ", "\n\n", ""];
        let mut out = String::new();
        if rng.below(4) == 0 {
            out.push_str(RUNS[rng.below(RUNS.len() - 1)]);
        }
        let mut in_run = false;
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !in_run {
                    out.push_str(RUNS[rng.below(RUNS.len())]);
                }
                in_run = true;
            } else {
                // Sometimes break a line between two letters, too.
                if !in_run && !out.is_empty() && rng.below(10) == 0 {
                    out.push('\n');
                }
                out.push(ch);
                in_run = false;
            }
        }
        if rng.below(4) == 0 {
            out.push_str(RUNS[rng.below(RUNS.len() - 1)]);
        }
        out
    }

    /// Change, drop or add one non-whitespace character.
    fn mutate(rng: &mut Rng, text: &str) -> String {
        let positions = text
            .char_indices()
            .filter(|(_, ch)| !ch.is_whitespace())
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let Some(&at) = positions.get(rng.below(positions.len().max(1))) else {
            return format!("{text}z");
        };
        let ch = text[at..].chars().next().unwrap();
        let rest = &text[at + ch.len_utf8()..];
        match rng.below(3) {
            0 => format!("{}z{rest}", &text[..at]),
            1 => format!("{}{rest}", &text[..at]),
            _ => format!("{}a{ch}{rest}", &text[..at]),
        }
    }

    /// A needle for `lines`: a window of them reflowed, a reflowed window
    /// with one character changed, a verbatim window, or random pieces.
    /// Windows often start at the first line or end at the last.
    pub fn needle_text(rng: &mut Rng, lines: &[String]) -> String {
        let start = match rng.below(4) {
            0 => 0,
            _ => rng.below(lines.len()),
        };
        let end = match rng.below(4) {
            0 => lines.len(),
            _ => start + 1 + rng.below(lines.len() - start),
        };
        let window = lines[start..end].join("\n");
        match rng.below(6) {
            0 | 1 | 2 => reflow(rng, &window),
            3 => {
                let reflowed = reflow(rng, &window);
                mutate(rng, &reflowed)
            }
            4 => window,
            _ => (0..1 + rng.below(4))
                .map(|_| TOKENS[rng.below(TOKENS.len())])
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn stripped(text: &str) -> String {
        text.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    fn normalized(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Every `(start, end)` line window whose first and last lines have
    /// content and whose whitespace-normalized text is exactly `needle`, by
    /// checking all windows.
    pub fn brute_force_candidates(lines: &[&str], needle: &str) -> Vec<(usize, usize)> {
        let mut candidates = Vec::new();
        for start in 0..lines.len() {
            for end in start + 1..=lines.len() {
                if stripped(lines[start]).is_empty() || stripped(lines[end - 1]).is_empty() {
                    continue;
                }
                if normalized(&lines[start..end].join("\n")) == needle {
                    candidates.push((start, end));
                }
            }
        }
        candidates
    }

    /// Windows with content at both ends whose non-whitespace text equals
    /// `needle`'s but whose spacing differs: the near misses a search on
    /// stripped text alone would have had to reject.
    pub fn spacing_misses(lines: &[&str], needle: &str) -> usize {
        let (needle_stripped, needle_normalized) = (stripped(needle), normalized(needle));
        let mut misses = 0;
        for start in 0..lines.len() {
            for end in start + 1..=lines.len() {
                if stripped(lines[start]).is_empty() || stripped(lines[end - 1]).is_empty() {
                    continue;
                }
                let window = lines[start..end].join("\n");
                if stripped(&window) == needle_stripped && normalized(&window) != needle_normalized
                {
                    misses += 1;
                }
            }
        }
        misses
    }

    /// Occurrences of the normalized `needle` anywhere in the lines'
    /// normalized text, aligned to line boundaries or not.
    pub fn stream_occurrences(lines: &[&str], needle: &str) -> usize {
        let stream = normalized(&lines.join("\n"));
        (0..stream.len())
            .filter(|&at| stream.is_char_boundary(at) && stream[at..].starts_with(needle))
            .count()
    }
}

#[cfg(test)]
mod reflow_linear {
    use super::reflow_test_support::*;
    use super::*;

    const REFLOW_NON_WS_TOLERANCE: usize = 8;

    /// The window-extending scan `find_reflow_match` used before the linear
    /// candidate search, kept unchanged as the reference result.
    fn find_reflow_match_scan_oracle(
        lines: &[&str],
        pattern: &[&str],
        start_index: usize,
    ) -> Option<(usize, usize)> {
        let needle_text = pattern.join("\n");
        let normalized_needle = normalize_reflow_whitespace(&needle_text);
        let needle_non_whitespace = strip_reflow_whitespace(&needle_text);
        if normalized_needle.is_empty() || needle_non_whitespace.is_empty() {
            return None;
        }
        let needle_non_whitespace_len = needle_non_whitespace.chars().count();
        let min_non_whitespace = needle_non_whitespace_len.saturating_sub(REFLOW_NON_WS_TOLERANCE);
        let max_non_whitespace = needle_non_whitespace_len + REFLOW_NON_WS_TOLERANCE;
        let mut first_match = None;
        let line_lengths: Vec<usize> = lines
            .iter()
            .map(|line| line.chars().filter(|ch| !ch.is_whitespace()).count())
            .collect();
        for start in start_index..lines.len() {
            if !has_reflow_content(lines[start]) {
                continue;
            }
            let mut window_non_whitespace_len = 0;
            for end in (start + 1)..=lines.len() {
                let line = lines[end - 1];
                window_non_whitespace_len += line_lengths[end - 1];
                if window_non_whitespace_len > max_non_whitespace {
                    break;
                }
                if window_non_whitespace_len < min_non_whitespace {
                    continue;
                }
                if !has_reflow_content(line) {
                    continue;
                }
                let window_chars = || {
                    lines[start..end]
                        .iter()
                        .flat_map(|line| line.chars().chain(std::iter::once('\n')))
                };
                if !window_chars()
                    .filter(|ch| !ch.is_whitespace())
                    .eq(needle_non_whitespace.chars())
                {
                    continue;
                }
                if !collapsed_whitespace(window_chars()).eq(normalized_needle.chars()) {
                    continue;
                }
                if first_match.replace((start, end - start)).is_some() {
                    return None;
                }
            }
        }
        first_match
    }

    /// The linear search returns what the old scan returned on generated
    /// inputs covering whitespace-only lines, tabs, CRLF, repeated blocks,
    /// overlapping candidates, near-miss needles (including ones with the
    /// same text but different spacing) and needles at either end of the
    /// file. Its window list also equals a brute-force list of every
    /// window, which pins the line-boundary alignment directly.
    #[test]
    fn linear_reflow_match_equals_the_window_scan_on_generated_inputs() {
        let mut rng = Rng::new(0x5eed_0001);
        let (mut found, mut spacing_near_misses, mut misaligned) = (0, 0, 0);
        for case in 0..10_000 {
            let mut lines = file_lines(&mut rng);
            if rng.below(3) == 0 {
                for line in &mut lines {
                    line.push('\r');
                }
            }
            let needle = needle_text(&mut rng, &lines);
            let pattern = needle.split('\n').collect::<Vec<_>>();
            let lines = lines.iter().map(String::as_str).collect::<Vec<_>>();
            let start_index = rng.below(lines.len() + 1);
            let expected = find_reflow_match_scan_oracle(&lines, &pattern, start_index);
            let actual = find_reflow_match(&lines, &pattern, start_index);
            assert_eq!(
                actual, expected,
                "case {case}: lines {lines:?} pattern {pattern:?} start {start_index}"
            );
            found += usize::from(actual.is_some());

            let normalized = normalize_reflow_whitespace(&needle);
            if normalized.is_empty() {
                continue;
            }
            let candidates = ReflowCandidates::new(&lines, &normalized).collect::<Vec<_>>();
            assert_eq!(
                candidates,
                brute_force_candidates(&lines, &normalized),
                "case {case}: lines {lines:?} needle {needle:?}"
            );
            spacing_near_misses += spacing_misses(&lines, &needle);
            misaligned += stream_occurrences(&lines, &normalized) - candidates.len();
        }
        // The generator must exercise every branch the oracle comparison
        // relies on, or an equal result proves little.
        assert!(found > 300, "only {found} matches");
        assert!(
            spacing_near_misses > 300,
            "only {spacing_near_misses} spacing near misses"
        );
        assert!(misaligned > 300, "only {misaligned} misaligned occurrences");
    }

    // BEGIN reflow-work-bound test
    /// A repetitive file whose lines all match the hunk's text, but the
    /// hunk's last line differs only in spacing ("ab" for "a b"). Every
    /// whitespace-stripped occurrence was a spacing near miss that used to be
    /// rescanned in full before being rejected: about 18,000 windows of 28 KB.
    /// The whole pass, building its stream, scanning it and normalizing or
    /// comparing any text, stays within a small multiple of the file plus
    /// hunk size.
    #[test]
    fn reflow_pass_work_is_linear_on_repetitive_spacing_misses() {
        let lines = vec!["a b"; 25_000];
        let mut pattern = vec!["a b"; 6_999];
        pattern.push("ab");
        REFLOW_WORK.with(|work| work.set(0));
        assert_eq!(find_reflow_match(&lines, &pattern, 0), None);
        let work = REFLOW_WORK.with(|work| work.get());
        let size = lines.len() * "a b\n".len() + pattern.join("\n").len();
        assert!(work <= 4 * size, "{work} units of work for {size} bytes");
    }
    // END reflow-work-bound test

    /// The candidate scan does at most two steps per stream byte however the
    /// file repeats: here 25,000 identical lines against a 7,000-line needle,
    /// where every line start begins a long partial match.
    #[test]
    fn reflow_candidate_scan_is_linear_on_repetitive_input() {
        let lines = vec!["    value += 1;"; 25_000];
        let mut pattern = vec!["value += 1;"; 7_000];
        pattern.push("value += 2;");
        // The normalized stream: each line's text and one separating space.
        let stream_bytes = lines.len() * "value += 1; ".len();
        REFLOW_SCAN_STEPS.with(|steps| steps.set(0));
        assert_eq!(find_reflow_match(&lines, &pattern, 0), None);
        let steps = REFLOW_SCAN_STEPS.with(|steps| steps.get());
        assert!(
            steps <= 2 * stream_bytes,
            "{steps} steps for {stream_bytes} bytes"
        );
    }
}

/// Generated inputs for the line-tier differential tests here and in
/// `fuzzy_match`: lines with duplicates, empty and whitespace-only lines,
/// leading and trailing spaces and tabs, smart quotes and dashes.
#[cfg(any(test, feature = "test-support"))]
pub mod line_test_support {
    use super::reflow_test_support::Rng;

    const LINES: &[&str] = &[
        "a",
        "a ",
        " a",
        "\ta",
        "a\t",
        "  a  ",
        "b",
        "b ",
        "",
        " ",
        "\t",
        "x y",
        "x  y",
        "\u{2018}q\u{2019}",
        "'q'",
        "\u{2014}",
        "-",
        "a b",
    ];

    /// File lines: random lines, often with a repeated block, so that
    /// matches repeat and overlap.
    pub fn file_lines(rng: &mut Rng) -> Vec<String> {
        let count = 1 + rng.below(16);
        let mut lines = Vec::new();
        while lines.len() < count {
            if rng.below(3) == 0 {
                let block = (0..1 + rng.below(3))
                    .map(|_| LINES[rng.below(LINES.len())].to_string())
                    .collect::<Vec<_>>();
                for _ in 0..2 + rng.below(4) {
                    lines.extend(block.iter().cloned());
                }
            } else {
                lines.push(LINES[rng.below(LINES.len())].to_string());
            }
        }
        lines.truncate(count);
        lines
    }

    /// Pattern lines: a window of the file with each line kept, given extra
    /// leading or trailing whitespace, or replaced; or random lines.
    pub fn pattern_lines(rng: &mut Rng, lines: &[String]) -> Vec<String> {
        if rng.below(5) == 0 {
            return (0..1 + rng.below(4))
                .map(|_| LINES[rng.below(LINES.len())].to_string())
                .collect();
        }
        let start = if rng.below(4) == 0 {
            0
        } else {
            rng.below(lines.len())
        };
        let end = if rng.below(4) == 0 {
            lines.len()
        } else {
            start + 1 + rng.below(lines.len() - start)
        };
        lines[start..end]
            .iter()
            .map(|line| match rng.below(8) {
                0 => format!("{line} "),
                1 => format!(" {line}"),
                2 => format!("{line}\t"),
                3 => LINES[rng.below(LINES.len())].to_string(),
                _ => line.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod line_tiers {
    use super::line_test_support::{file_lines, pattern_lines};
    use super::reflow_test_support::Rng;
    use super::*;

    /// `try_match` as it was before the Knuth-Morris-Pratt scan: every start
    /// tried in turn. Kept unchanged as the reference result.
    fn try_match_scan_oracle<F>(
        lines: &[&str],
        pattern: &[&str],
        start_index: usize,
        compare: F,
        eof: bool,
    ) -> Option<usize>
    where
        F: Fn(&str, &str) -> bool,
    {
        let matches_at = |start: usize| {
            pattern
                .iter()
                .enumerate()
                .all(|(offset, expected)| compare(lines[start + offset], expected))
        };
        if pattern.is_empty() || pattern.len() > lines.len() {
            return None;
        }
        if eof {
            let from_end = lines.len() - pattern.len();
            if from_end >= start_index && matches_at(from_end) {
                return Some(from_end);
            }
            return None;
        }
        let last_start = lines.len() - pattern.len();
        if start_index > last_start {
            return None;
        }
        (start_index..=last_start).find(|&start| matches_at(start))
    }

    /// The first match and the next one after it (the uniqueness check the
    /// non-exact tiers make) agree with the old scan on generated inputs.
    fn assert_tier_matches_scan(seed: u64, compare: fn(&str, &str) -> bool) {
        let mut rng = Rng::new(seed);
        let mut found = 0;
        for case in 0..10_000 {
            let mut lines = file_lines(&mut rng);
            let mut pattern = pattern_lines(&mut rng, &lines);
            if rng.below(4) == 0 {
                // apply_patch strips a CR before matching; an unstripped one
                // must still compare the same way in both scans.
                lines.iter_mut().for_each(|line| line.push('\r'));
                if rng.below(2) == 0 {
                    pattern.iter_mut().for_each(|line| line.push('\r'));
                }
            }
            let lines = lines.iter().map(String::as_str).collect::<Vec<_>>();
            let pattern = pattern.iter().map(String::as_str).collect::<Vec<_>>();
            let start = rng.below(lines.len() + 1);
            let eof = rng.below(4) == 0;
            let expected = try_match_scan_oracle(&lines, &pattern, start, compare, eof);
            let actual = try_match(&lines, &pattern, start, compare, eof);
            assert_eq!(
                actual, expected,
                "case {case}: lines {lines:?} pattern {pattern:?} start {start} eof {eof}"
            );
            if let Some(first) = actual {
                found += 1;
                assert_eq!(
                    try_match(&lines, &pattern, first + 1, compare, eof),
                    try_match_scan_oracle(&lines, &pattern, first + 1, compare, eof),
                    "case {case}: next match after {first}"
                );
            }
        }
        assert!(found > 500, "only {found} matches");
    }

    #[test]
    fn exact_tier_matches_the_start_by_start_scan() {
        assert_tier_matches_scan(0x11ae_0001, |a, b| a == b);
    }

    #[test]
    fn rstrip_tier_matches_the_start_by_start_scan() {
        assert_tier_matches_scan(0x11ae_0002, |a, b| a.trim_end() == b.trim_end());
    }

    #[test]
    fn trim_tier_matches_the_start_by_start_scan() {
        assert_tier_matches_scan(0x11ae_0003, |a, b| a.trim() == b.trim());
    }

    // BEGIN line-tier work-bound test
    /// Every line of a repetitive file matches the hunk's lines except its
    /// last, which differs only in spacing ("ab" for "a b"). Trying every
    /// start compared 7,000 lines (6,999 equal, then the differing last one)
    /// at each of 18,001 starts in the exact, rstrip and trim tiers. All line tiers together now stay within
    /// a small multiple of the file plus hunk size.
    #[test]
    fn line_tier_work_is_linear_on_repetitive_near_misses() {
        let lines = vec!["a b"; 25_000];
        let mut pattern = vec!["a b"; 6_999];
        pattern.push("ab");
        LINE_MATCH_WORK.with(|work| work.set(0));
        assert_eq!(seek_sequence_tiered(&lines, &pattern, 0, false), None);
        let work = LINE_MATCH_WORK.with(|work| work.get());
        let size = lines.len() * "a b\n".len() + pattern.join("\n").len();
        assert!(work <= 32 * size, "{work} units of work for {size} bytes");
    }
    // END line-tier work-bound test
}

#[cfg(test)]
mod approved_audit11 {
    use super::*;

    #[test]
    fn non_exact_matches_must_be_unique_in_the_eligible_range() {
        for (lines, pattern) in [
            (vec!["target ", "other", "target\t"], vec!["target"]),
            (vec!["  target", "other", "\ttarget"], vec!["target"]),
            (vec!["‘target’", "other", "’target’"], vec!["'target'"]),
        ] {
            assert!(
                seek_sequence_tiered(&lines, &pattern, 0, false).is_none(),
                "{lines:?}"
            );
            let selected = seek_sequence_tiered(&lines, &pattern, 2, false).unwrap();
            assert_eq!(selected.found, 2);
            assert_eq!(
                seek_sequence_tiered(&lines, &pattern, 0, true)
                    .unwrap()
                    .found,
                2
            );
        }
        assert!(
            seek_sequence_tiered(
                &["say( ‘a’ )", "say( ’a’ )", "say(", " 'a' ", ")"],
                &["say( 'a' )"],
                0,
                false
            )
            .is_none(),
            "an ambiguous stronger fallback must not fall through to reflow"
        );
        assert_eq!(
            seek_sequence_tiered(&["target", "target"], &["target"], 0, false)
                .unwrap()
                .found,
            0
        );
    }
}
