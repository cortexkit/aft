//! Fuzzy string matching for edit_match, inspired by opencode's 4-pass approach.
//!
//! When exact matching fails, progressively relaxes comparison:
//!   Pass 1: Exact match (str::find / match_indices)
//!   Pass 2: Trim trailing whitespace per line
//!   Pass 3: Trim both ends per line
//!   Pass 4: Normalize Unicode punctuation + trim
//!   Pass 5: Reflowed line wraps/joins with whitespace-normalized content

use crate::patch::matcher::{find_nearest_miss, NearestMissSearch, ReflowCandidates};

// A half-match floor keeps diagnostics useful for a likely stale edit while
// suppressing suggestions for candidates that share too little of the needle.
pub(crate) const NEAREST_MISS_SIMILARITY_FLOOR: f32 = 0.5;
// Ten candidate lines provide context without turning an error into a file dump.
pub(crate) const NEAREST_MISS_EXCERPT_LINES: usize = 10;
// Eight occurrence entries are enough to choose a match while bounding error text.
pub(crate) const AMBIGUOUS_OCCURRENCE_LIST_LIMIT: usize = 8;
// Long generated lines need a byte-independent cap so one line cannot dominate the detail.
pub(crate) const NEAREST_MISS_LINE_CHARS: usize = 240;

/// A match result: byte offset in source and the matched byte length.
#[derive(Debug, Clone)]
pub struct FuzzyMatch {
    pub byte_start: usize,
    pub byte_len: usize,
    /// Which pass found the match (1=exact, 2=rstrip, 3=trim, 4=unicode, 5=reflow)
    pub pass: u8,
}

/// Find all occurrences of `needle` in `haystack` using progressive fuzzy matching.
/// Returns matches in order of their byte position in the source.
pub fn find_all_fuzzy(haystack: &str, needle: &str) -> Vec<FuzzyMatch> {
    // Pass 1: exact match (fast path)
    let exact: Vec<FuzzyMatch> = haystack
        .match_indices(needle)
        .map(|(idx, _)| FuzzyMatch {
            byte_start: idx,
            byte_len: needle.len(),
            pass: 1,
        })
        .collect();

    if !exact.is_empty() {
        return exact;
    }

    // For fuzzy passes, work line-by-line
    let needle_lines: Vec<&str> = needle.lines().collect();
    if needle_lines.is_empty() {
        return vec![];
    }

    let haystack_lines: Vec<&str> = haystack.lines().collect();
    let line_byte_offsets = compute_line_offsets(haystack);

    // Pass 2: rstrip (trim trailing whitespace)
    let rstrip_matches = find_line_matches(
        &haystack_lines,
        &needle_lines,
        &line_byte_offsets,
        haystack,
        needle,
        |a, b| a.trim_end() == b.trim_end(),
        2,
    );
    if !rstrip_matches.is_empty() {
        return rstrip_matches;
    }

    // Pass 3: trim (both ends)
    let trim_matches = find_line_matches(
        &haystack_lines,
        &needle_lines,
        &line_byte_offsets,
        haystack,
        needle,
        |a, b| a.trim() == b.trim(),
        3,
    );
    if !trim_matches.is_empty() {
        return trim_matches;
    }

    // Pass 4: normalized Unicode + trim. Normalize each line once instead of
    // allocating inside the O(haystack_lines × needle_lines) comparison loop.
    let normalized_haystack_lines: Vec<String> = haystack_lines
        .iter()
        .map(|line| normalize_unicode(line.trim()))
        .collect();
    let normalized_needle_lines: Vec<String> = needle_lines
        .iter()
        .map(|line| normalize_unicode(line.trim()))
        .collect();
    let normalized_haystack_refs: Vec<&str> = normalized_haystack_lines
        .iter()
        .map(String::as_str)
        .collect();
    let normalized_needle_refs: Vec<&str> =
        normalized_needle_lines.iter().map(String::as_str).collect();
    let normalized_matches = find_line_matches(
        &normalized_haystack_refs,
        &normalized_needle_refs,
        &line_byte_offsets,
        haystack,
        needle,
        |a, b| a == b,
        4,
    );
    if !normalized_matches.is_empty() {
        return normalized_matches;
    }

    // Pass 5: final fallback for formatter reflows. This pass deliberately
    // runs only after every line-contiguous pass fails, and each candidate
    // window must have the same non-whitespace content as the needle.
    find_reflow_matches(
        &haystack_lines,
        &needle_lines,
        &line_byte_offsets,
        haystack,
        needle,
    )
}

pub(crate) fn render_nearest_miss_detail(source: &str, needle: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let pattern: Vec<&str> = needle.lines().collect();
    let fallback = format!(" (file has {} lines)", lines.len());

    let nearest = match find_nearest_miss(&lines, &pattern, source.len()) {
        NearestMissSearch::Found(nearest) => nearest,
        NearestMissSearch::NoSimilarRegion | NearestMissSearch::SkippedLargeFile => {
            return fallback;
        }
    };
    let similarity = nearest.matched_lines as f32 / pattern.len().max(1) as f32;
    if similarity < NEAREST_MISS_SIMILARITY_FLOOR {
        return fallback;
    }

    let start_line = nearest.start + 1;
    let end_line = nearest.end;
    let mut detail = format!("\nNearest candidate at lines {start_line}-{end_line}:");
    let available_lines = nearest.end.saturating_sub(nearest.start);
    let line_number_width = end_line.to_string().len();

    if available_lines <= NEAREST_MISS_EXCERPT_LINES {
        for (offset, line) in lines[nearest.start..nearest.end].iter().enumerate() {
            append_diagnostic_line(&mut detail, nearest.start, offset, line, line_number_width);
        }
    } else {
        let head_lines = NEAREST_MISS_EXCERPT_LINES / 2;
        let tail_lines = NEAREST_MISS_EXCERPT_LINES - head_lines;
        for (offset, line) in lines[nearest.start..nearest.start + head_lines]
            .iter()
            .enumerate()
        {
            append_diagnostic_line(&mut detail, nearest.start, offset, line, line_number_width);
        }
        detail.push_str("\n  ... (middle candidate lines truncated)");
        for (offset, line) in lines[nearest.end - tail_lines..nearest.end]
            .iter()
            .enumerate()
        {
            append_diagnostic_line(
                &mut detail,
                nearest.start,
                available_lines - tail_lines + offset,
                line,
                line_number_width,
            );
        }
    }

    let divergence = nearest.first_divergence;
    let expected = pattern.get(divergence).copied().unwrap_or("<EOF>");
    let actual = lines
        .get(nearest.start + divergence)
        .copied()
        .unwrap_or("<EOF>");
    detail.push_str(&format!(
        "\nFirst divergence:\n- expected: {}\n+ actual: {}",
        shorten_diagnostic_line(expected),
        shorten_diagnostic_line(actual),
    ));
    detail
}

/// Longest searched text a not-found error repeats as-is.
const SEARCHED_TEXT_ECHO_CHARS: usize = 120;

/// How a not-found error names the text it searched for. One short line is
/// quoted as-is. Anything longer shows only its first line, cut to
/// [`SEARCHED_TEXT_ECHO_CHARS`], and its size: the caller already has the
/// text, and repeating a large one doubles its cost in the transcript.
pub(crate) fn describe_searched_text(text: &str) -> String {
    if !text.contains('\n') && text.chars().count() <= SEARCHED_TEXT_ECHO_CHARS {
        return format!("'{text}'");
    }
    let first: String = text
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(SEARCHED_TEXT_ECHO_CHARS)
        .collect();
    let lines = text.lines().count();
    let noun = if lines == 1 { "line" } else { "lines" };
    format!("'{first}…' ({lines} {noun}, {} bytes)", text.len())
}

pub(crate) fn render_edit_not_found_detail(
    source: &str,
    old_string: &str,
    new_string: &str,
) -> String {
    let mut detail = render_nearest_miss_detail(source, old_string);
    if new_string.is_empty() || new_string.trim().is_empty() || old_string.contains(new_string) {
        return detail;
    }

    let mut matches = source.match_indices(new_string);
    let Some((first_match, _)) = matches.next() else {
        return detail;
    };
    if matches.next().is_some() {
        let count = 2 + matches.count();
        detail.push_str(&format!(
            "\nnewString occurs {count} times; the edit may already be applied."
        ));
    } else {
        let line = source.as_bytes()[..first_match]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
            + 1;
        detail.push_str(&format!(
            "\nnewString is already present at line {line}; the edit may already be applied."
        ));
    }
    detail
}

fn append_diagnostic_line(
    detail: &mut String,
    start: usize,
    offset: usize,
    line: &str,
    line_number_width: usize,
) {
    let line_number = start + offset + 1;
    detail.push_str(&format!(
        "\n  {line_number:>line_number_width$} | {}",
        shorten_diagnostic_line(line),
        line_number_width = line_number_width,
    ));
}

fn shorten_diagnostic_line(line: &str) -> String {
    let chars = line.chars().collect::<Vec<_>>();
    if chars.len() <= NEAREST_MISS_LINE_CHARS {
        return line.to_string();
    }

    let head = NEAREST_MISS_LINE_CHARS / 2;
    let tail = NEAREST_MISS_LINE_CHARS - head;
    let mut shortened = chars[..head].iter().collect::<String>();
    shortened.push('…');
    shortened.extend(chars[chars.len() - tail..].iter());
    shortened
}

/// Render a bounded 1-based occurrence list for an ambiguity error.
pub(crate) fn render_occurrence_listing(source: &str, positions: &[usize]) -> String {
    let listed = positions
        .iter()
        .take(AMBIGUOUS_OCCURRENCE_LIST_LIMIT)
        .enumerate()
        .map(|(index, position)| {
            let line = source[0..*position].matches('\n').count() + 1;
            format!("#{} at line {}", index + 1, line)
        })
        .collect::<Vec<_>>();
    let mut detail = format!(" {} occurrences: {}", positions.len(), listed.join(", "));
    if positions.len() > AMBIGUOUS_OCCURRENCE_LIST_LIMIT {
        detail.push_str(&format!(
            ", … and {} more",
            positions.len() - AMBIGUOUS_OCCURRENCE_LIST_LIMIT
        ));
    }
    detail
}

/// Compute byte offset of each line start in the source string.
fn compute_line_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (i, c) in source.char_indices() {
        if c == '\n' && i + 1 <= source.len() {
            offsets.push(i + 1);
        }
    }
    offsets
}

/// Find all positions where `needle_lines` matches a contiguous sequence in `haystack_lines`.
///
/// `compare` must be an equivalence relation on lines (equality of a key such
/// as the trimmed line). The search is a Knuth-Morris-Pratt scan over lines
/// that reports every occurrence, overlapping ones included, in ascending
/// order, so it is linear in haystack plus needle lines. Trying every start
/// cost haystack lines × needle lines when long runs of lines repeat.
fn find_line_matches<F>(
    haystack_lines: &[&str],
    needle_lines: &[&str],
    line_offsets: &[usize],
    haystack: &str,
    needle: &str,
    compare: F,
    pass: u8,
) -> Vec<FuzzyMatch>
where
    F: Fn(&str, &str) -> bool,
{
    let compare = |line: &str, expected: &str| {
        #[cfg(test)]
        crate::patch::matcher::count_line_comparison(line, expected);
        compare(line, expected)
    };
    let mut matches = Vec::new();
    if needle_lines.is_empty() || needle_lines.len() > haystack_lines.len() {
        return matches;
    }

    // prefix[i]: length of the longest proper prefix of needle_lines[..=i]
    // that is also its suffix, under `compare`.
    let mut prefix = vec![0; needle_lines.len()];
    for i in 1..needle_lines.len() {
        let mut matched = prefix[i - 1];
        loop {
            if compare(needle_lines[i], needle_lines[matched]) {
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
    for (index, line) in haystack_lines.iter().enumerate() {
        loop {
            if compare(line, needle_lines[matched]) {
                matched += 1;
                break;
            }
            if matched == 0 {
                break;
            }
            matched = prefix[matched - 1];
        }
        if matched < needle_lines.len() {
            continue;
        }
        matched = prefix[matched - 1];
        // Found a match at line `i` spanning `needle_lines.len()` lines
        let i = index + 1 - needle_lines.len();
        let byte_start = line_offsets[i];
        let end_line = i + needle_lines.len();
        let byte_end = if end_line < line_offsets.len() {
            // Include the newline after the last matched line
            line_offsets[end_line]
        } else {
            haystack.len()
        };
        matches.push(boundary_match(haystack, needle, byte_start, byte_end, pass));
    }

    matches
}

// Match line windows for comparison, but leave whitespace outside the caller's
// boundary text in place. In particular a missing terminal newline is not an
// instruction to remove the line separator.
fn boundary_match(source: &str, needle: &str, start: usize, end: usize, pass: u8) -> FuzzyMatch {
    let window = &source[start..end];
    let source_first = window.lines().next().unwrap_or("");
    let needle_first = needle.lines().next().unwrap_or("");
    let source_lead = source_first
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let needle_lead = needle_first
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let leading_extra = source_lead.saturating_sub(needle_lead);
    let byte_start = start
        + source_first
            .chars()
            .take(leading_extra)
            .map(char::len_utf8)
            .sum::<usize>();

    let source_last = window
        .trim_end_matches('\n')
        .split('\n')
        .next_back()
        .unwrap_or("");
    let needle_last = needle
        .trim_end_matches('\n')
        .split('\n')
        .next_back()
        .unwrap_or("");
    let source_tail = source_last
        .chars()
        .rev()
        .take_while(|c| c.is_whitespace())
        .count();
    let needle_tail = needle_last
        .chars()
        .rev()
        .take_while(|c| c.is_whitespace())
        .count();
    let trailing_extra = source_tail.saturating_sub(needle_tail);
    let mut byte_end = end;
    if window.ends_with('\n') && (!needle.ends_with('\n') || trailing_extra > 0) {
        byte_end -= 1;
    }
    byte_end -= source_last
        .chars()
        .rev()
        .take(trailing_extra)
        .map(char::len_utf8)
        .sum::<usize>();
    FuzzyMatch {
        byte_start,
        // Whitespace-only windows can trim the same characters from both ends.
        byte_len: byte_end.saturating_sub(byte_start),
        pass,
    }
}

/// Describe fuzzy replacements only when the bytes being replaced contain
/// non-whitespace content different from the requested text. `notice` is the
/// reply line that tells the agent the match was loose (see
/// [`loose_match_notice`]); `reindented` says whether any replacement was
/// shifted to the file's indentation.
pub fn replacement_detail(source: &str, needle: &str, matches: &[FuzzyMatch]) -> serde_json::Value {
    let pass = matches.first().map(|m| m.pass).unwrap_or(1);
    // A refusal never reaches here: the replacement was already built.
    let reindented = matches.iter().any(|m| {
        indentation_shift(source, needle, m).is_ok_and(|shift| shift != IndentShift::None)
    });
    let actual: Vec<String> = matches
        .iter()
        .filter_map(|m| {
            let text = &source[m.byte_start..m.byte_start + m.byte_len];
            let content = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            (content(text) != content(needle)).then(|| {
                let escaped = text.escape_debug().to_string();
                let excerpt: String = escaped.chars().take(160).collect();
                if escaped.chars().count() > 160 {
                    format!("{excerpt}…")
                } else {
                    excerpt
                }
            })
        })
        .collect();
    let mut detail = serde_json::json!({
        "pass": pass,
        "replaced_text": actual,
        "reindented": reindented,
    });
    if let Some(notice) = loose_match_notice(pass, reindented) {
        detail["notice"] = serde_json::json!(notice);
    }
    detail
}

/// The one-line reply notice for an edit whose `oldString` was found only by
/// a loose pass: what the match ignored, and whether `newString` was shifted
/// to the file's indentation. This is the only place the wording lives; the
/// reply formatters render the `notice` field as given.
pub(crate) fn loose_match_notice(pass: u8, reindented: bool) -> Option<String> {
    let ignored = match pass {
        2 => "trailing whitespace",
        3 => "indentation and trailing whitespace",
        4 => "indentation, trailing whitespace and Unicode punctuation (curly quotes, dashes, ellipses, non-breaking spaces)",
        5 => "line breaks and spacing",
        _ => return None,
    };
    Some(if reindented {
        format!("oldString matched only after ignoring {ignored}; newString was re-indented to match the file.")
    } else {
        format!("oldString matched only after ignoring {ignored}.")
    })
}

/// How a loose match's replacement has to move to sit at the file's
/// indentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IndentShift {
    /// The matched lines and `oldString` share their indentation.
    None,
    /// Every matched line carries this extra outer prefix that the
    /// corresponding `oldString` line lacks (`oldString` was written too
    /// shallow), so each replacement line gets it too.
    Add(String),
    /// Every `oldString` line carries this extra outer prefix that the
    /// matched line lacks (`oldString` was written too deep), so each
    /// replacement line loses it.
    Remove(String),
}

fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

const COPY_EXACT_INDENTATION: &str =
    "Nothing was changed; copy oldString with the file's exact indentation.";

fn ambiguous_indentation(reason: String) -> crate::error::AftError {
    crate::error::AftError::AmbiguousIndentation {
        message: format!(
            "oldString matched only after ignoring indentation, but {reason}, so newString cannot be re-indented without guessing. {COPY_EXACT_INDENTATION}"
        ),
    }
}

/// 1-based file line of the line that starts at `line_start`, plus `offset`.
fn file_line(source: &str, line_start: usize, offset: usize) -> usize {
    source[..line_start].matches('\n').count() + 1 + offset
}

/// The indentation shift for a match found by a pass that ignores leading
/// whitespace (3: trimmed lines, 4: trimmed lines with Unicode punctuation
/// normalised). Those passes pair each `oldString` line with one file line,
/// so the difference in their leading whitespace is measurable. Other passes
/// either keep leading whitespace exact (1, 2) or have no line pairing (5),
/// and never shift.
///
/// Whitespace is compared as text, never converted between tabs and spaces:
/// one leading-whitespace string must be the other plus an outer prefix, and
/// that prefix must be the same on every non-blank line. Anything else is
/// refused with `ambiguous_indentation`, because any shift would be a guess.
pub(crate) fn indentation_shift(
    source: &str,
    needle: &str,
    matched: &FuzzyMatch,
) -> Result<IndentShift, crate::error::AftError> {
    if !matches!(matched.pass, 3 | 4) {
        return Ok(IndentShift::None);
    }
    let line_start = source[..matched.byte_start]
        .rfind('\n')
        .map_or(0, |at| at + 1);
    let needle_lines: Vec<&str> = needle.lines().collect();
    let source_lines = source[line_start..].lines().take(needle_lines.len());
    let mut shift: Option<IndentShift> = None;
    for (offset, (file_text, needle_text)) in source_lines.zip(&needle_lines).enumerate() {
        // Lines matched after trimming are either both blank or both not.
        if file_text.trim().is_empty() || needle_text.trim().is_empty() {
            continue;
        }
        let (file_lead, needle_lead) = (
            leading_whitespace(file_text),
            leading_whitespace(needle_text),
        );
        let line_shift = if file_lead == needle_lead {
            IndentShift::None
        } else if let Some(prefix) = file_lead.strip_suffix(needle_lead) {
            IndentShift::Add(prefix.to_string())
        } else if let Some(prefix) = needle_lead.strip_suffix(file_lead) {
            IndentShift::Remove(prefix.to_string())
        } else {
            return Err(ambiguous_indentation(format!(
                "line {} of the file is indented with different whitespace (tabs versus spaces) than oldString",
                file_line(source, line_start, offset)
            )));
        };
        match &shift {
            None => shift = Some(line_shift),
            Some(expected) if *expected != line_shift => {
                return Err(ambiguous_indentation(format!(
                    "its indentation differs from the file's by different amounts on different lines (first at line {})",
                    file_line(source, line_start, offset)
                )));
            }
            Some(_) => {}
        }
    }
    Ok(shift.unwrap_or(IndentShift::None))
}

/// Node kinds holding a string literal whose text includes every byte of
/// its lines: Python strings, JS/TS template literals and strings, Rust and
/// Go raw strings, C strings, bash heredocs and quoted strings. Shifting a
/// line that starts inside one changes the string's value. Implicit
/// concatenations (`concatenated_string`) are excluded: the whitespace
/// between their parts is code, not string content.
fn is_string_literal_kind(kind: &str) -> bool {
    (kind.contains("string") && !kind.starts_with("concatenated")) || kind.contains("heredoc")
}

/// Text that can open or continue a string across lines in some language:
/// quotes, backticks, `<<` heredocs and Lua's `[[`. Used where no parse can
/// tell whether a line is inside a string, so a shift would be a guess.
fn has_string_delimiter(text: &str) -> bool {
    text.contains(['"', '\'', '`']) || text.contains("<<") || text.contains("[[")
}

/// For a language without a parser here: delimiters that open a multi-line
/// string in common languages (triple quotes, backticks, heredocs, Lua long
/// brackets), or a line with an odd number of double quotes, which leaves a
/// string open into the next line. Apostrophes are left out because prose
/// is full of them.
fn has_multiline_string_marker(text: &str) -> bool {
    text.contains("\"\"\"")
        || text.contains("\'\'\'")
        || text.contains('`')
        || text.contains("<<")
        || text.contains("[[")
        || text.lines().any(|line| line.matches('"').count() % 2 == 1)
}

/// Whether the tree has an ERROR or MISSING node overlapping `start..end`.
/// Inside such a region error recovery may have misread where strings
/// begin and end.
fn region_has_error(root: tree_sitter::Node<'_>, start: usize, end: usize) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.end_byte() < start || node.start_byte() > end {
            continue;
        }
        if node.is_error() || node.is_missing() {
            return true;
        }
        if node.has_error() {
            let mut cursor = node.walk();
            pending.extend(node.children(&mut cursor));
        }
    }
    false
}

/// The string literal (see [`is_string_literal_kind`]) that the line
/// beginning at byte `line_start` starts inside, if any. Such a line is part
/// of the string's value: adding or removing its leading whitespace changes
/// the string.
pub(crate) fn string_literal_at_line_start(
    root: tree_sitter::Node<'_>,
    line_start: usize,
) -> Option<tree_sitter::Node<'_>> {
    let mut node = root.descendant_for_byte_range(line_start, line_start);
    while let Some(current) = node {
        // A heredoc body or terminator can begin exactly at the line start;
        // every byte of it is still literal text.
        let starts_before = current.start_byte() < line_start
            || (current.start_byte() == line_start && current.kind().contains("heredoc"));
        if is_string_literal_kind(current.kind())
            && starts_before
            && line_start < current.end_byte()
        {
            return Some(current);
        }
        node = current.parent();
    }
    None
}

/// For each line of `text` (as split by `str::lines`), whether it begins
/// inside a multi-line string literal of `text`, parsed with `lang`. Code
/// generators that re-indent a block (inlining a function body) use it to
/// leave string contents alone. Text with no string delimiter is not parsed.
/// `None` when the boundaries cannot be determined: the parse fails, or it
/// has errors while string delimiters are present.
pub(crate) fn text_lines_inside_strings(
    text: &str,
    path: &std::path::Path,
    lang: crate::parser::LangId,
) -> Option<Vec<bool>> {
    let mut inside = vec![false; text.lines().count()];
    if !has_string_delimiter(text) {
        return Some(inside);
    }
    #[cfg(test)]
    STRING_LINE_PARSES.with(|count| count.set(count.get() + 1));
    let tree = crate::parser::parse_source_with_cached_parser(path, text, lang).ok()?;
    let root = tree.root_node();
    if region_has_error(root, 0, text.len()) {
        return None;
    }
    let mut line_start = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if index > 0 && string_literal_at_line_start(root, line_start).is_some() {
            inside[index] = true;
        }
        line_start += line.len();
    }
    Some(inside)
}

#[cfg(test)]
thread_local! {
    /// Full parses made to find which lines are inside string literals.
    pub(crate) static STRING_LINE_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Re-indents the replacement for each match of one find/replace (see
/// [`indentation_shift`]), sharing the work across matches. Finding which
/// replacement lines sit inside multi-line string literals needs a parse of
/// the edited file; a plan makes at most one such parse for all of its
/// matches, and only when some match needs shifting and a string delimiter
/// appears anywhere in the file, the matched lines or the replacement. A
/// `replace_all` over thousands of matches would otherwise parse the whole
/// file thousands of times.
pub(crate) struct ReindentPlan<'a> {
    source: &'a str,
    needle: &'a str,
    replacement: &'a str,
    /// The edited file, whose language decides what a string literal is;
    /// `None` for text without string literals (GitHub comments).
    path: Option<&'a std::path::Path>,
    /// The matches being replaced, ascending by start.
    matches: &'a [FuzzyMatch],
    source_has_delimiter: std::cell::OnceCell<bool>,
    /// Whether any two matches overlap, computed once per edit rather than
    /// rescanned for every match of a replace-all.
    overlapping: std::cell::OnceCell<bool>,
    shared: std::cell::OnceCell<Option<StringParse>>,
}

/// The edited file with every planned replacement inserted unshifted, its
/// parse, and where each replacement starts in that text. Moving lines that
/// are outside strings never changes where a string starts or ends, so this
/// parse holds for the shifted result too.
struct StringParse {
    text: String,
    tree: tree_sitter::Tree,
    replacement_starts: Vec<usize>,
}

impl<'a> ReindentPlan<'a> {
    pub(crate) fn new(
        source: &'a str,
        needle: &'a str,
        replacement: &'a str,
        path: Option<&'a std::path::Path>,
        matches: &'a [FuzzyMatch],
    ) -> Self {
        Self {
            source,
            needle,
            replacement,
            path,
            matches,
            source_has_delimiter: std::cell::OnceCell::new(),
            overlapping: std::cell::OnceCell::new(),
            shared: std::cell::OnceCell::new(),
        }
    }

    pub(crate) fn source(&self) -> &'a str {
        self.source
    }

    pub(crate) fn matched(&self, index: usize) -> &'a FuzzyMatch {
        &self.matches[index]
    }

    /// The whole lines the match at `index` covers.
    fn matched_text(&self, index: usize) -> &'a str {
        let matched = &self.matches[index];
        let start = self.source[..matched.byte_start]
            .rfind('\n')
            .map_or(0, |at| at + 1);
        &self.source[start..matched.byte_start + matched.byte_len]
    }

    /// Insert the unshifted replacement at each listed match and parse once.
    /// `None` when the parse fails.
    fn parse_with_replacements(
        &self,
        lang: crate::parser::LangId,
        indices: &[usize],
    ) -> Option<StringParse> {
        let path = self.path?;
        let mut text =
            String::with_capacity(self.source.len() + indices.len() * (self.replacement.len() + 1));
        let mut replacement_starts = Vec::with_capacity(indices.len());
        let mut copied_through = 0;
        for &index in indices {
            let matched = &self.matches[index];
            let end = matched.byte_start + matched.byte_len;
            text.push_str(&self.source[copied_through..matched.byte_start]);
            replacement_starts.push(text.len());
            text.push_str(self.replacement);
            // The edit restores a line separator the match consumed.
            if self.source[..end].ends_with('\n')
                && !self.replacement.is_empty()
                && !self.replacement.ends_with('\n')
            {
                text.push('\n');
            }
            copied_through = end;
        }
        text.push_str(&self.source[copied_through..]);
        #[cfg(test)]
        STRING_LINE_PARSES.with(|count| count.set(count.get() + 1));
        let tree = crate::parser::parse_source_with_cached_parser(path, &text, lang).ok()?;
        Some(StringParse {
            text,
            tree,
            replacement_starts,
        })
    }

    /// For each line of the replacement at match `index` (as split by
    /// `split_inclusive('\n')`), whether it starts inside a multi-line string
    /// literal that the replacement itself opens. Such a line is part of the
    /// string's value and keeps the exact text the agent wrote.
    ///
    /// Refuses with `ambiguous_indentation` when a line continues a string
    /// that opened before the edit (the agent's intended text for it is
    /// unknowable), and when it cannot tell: the parse is broken around the
    /// edit, or the language has no parser here, while string delimiters are
    /// present.
    fn lines_inside_new_strings(&self, index: usize) -> Result<Vec<bool>, crate::error::AftError> {
        let pieces: Vec<&str> = self.replacement.split_inclusive('\n').collect();
        let mut inside = vec![false; pieces.len()];
        let Some(path) = self.path else {
            return Ok(inside);
        };
        let matched_text = self.matched_text(index);
        let Some(lang) = crate::parser::detect_language(path) else {
            if has_multiline_string_marker(self.replacement)
                || has_multiline_string_marker(matched_text)
            {
                return Err(ambiguous_indentation(
                    "this file type has no parser here and newString or the matched lines may hold a multi-line string, whose text must not move".to_string(),
                ));
            }
            return Ok(inside);
        };
        let local_delimiters =
            has_string_delimiter(self.replacement) || has_string_delimiter(matched_text);
        // A quote anywhere earlier in the file can open a string the match
        // falls inside, so only a file with no delimiter at all is skipped.
        let source_has_delimiter = *self
            .source_has_delimiter
            .get_or_init(|| has_string_delimiter(self.source));
        if !local_delimiters && !source_has_delimiter {
            return Ok(inside);
        }
        let unsure = || {
            ambiguous_indentation(
                "the code around the edit does not parse, so lines inside multi-line strings cannot be told apart from code".to_string(),
            )
        };
        // Matches that overlap cannot all be inserted into one text; each is
        // parsed alone (the edit is rejected for the overlap anyway).
        let overlapping = *self.overlapping.get_or_init(|| {
            self.matches
                .windows(2)
                .any(|pair| pair[0].byte_start + pair[0].byte_len > pair[1].byte_start)
        });
        let single;
        let (parse, slot) = if overlapping {
            single = self.parse_with_replacements(lang, &[index]);
            (single.as_ref(), 0)
        } else {
            let shared = self.shared.get_or_init(|| {
                let all: Vec<usize> = (0..self.matches.len()).collect();
                self.parse_with_replacements(lang, &all)
            });
            (shared.as_ref(), index)
        };
        let Some(parse) = parse else {
            return if local_delimiters {
                Err(unsure())
            } else {
                Ok(inside)
            };
        };
        let replacement_start = parse.replacement_starts[slot];
        let root = parse.tree.root_node();
        if local_delimiters
            && region_has_error(
                root,
                replacement_start,
                replacement_start + self.replacement.len(),
            )
        {
            return Err(unsure());
        }
        let mut offset = replacement_start;
        for (line, piece) in pieces.iter().enumerate() {
            let line_start = offset;
            offset += piece.len();
            // Only lines that begin at a line start can be shifted.
            let at_line_start = line_start == 0 || parse.text.as_bytes()[line_start - 1] == b'\n';
            if !at_line_start || piece.trim().is_empty() {
                continue;
            }
            if let Some(string) = string_literal_at_line_start(root, line_start) {
                if string.start_byte() < replacement_start {
                    return Err(ambiguous_indentation(format!(
                        "line {} of newString is inside a multi-line string that starts before the edit, and that string's intended text is unclear",
                        line + 1
                    )));
                }
                inside[line] = true;
            }
        }
        Ok(inside)
    }

    /// The replacement for the match at `index`, with every line moved by
    /// the match's [`indentation_shift`]. A loose match keeps the file's
    /// indentation for the matched lines, so writing `newString` as given
    /// would leave every line after the first at `oldString`'s wrong
    /// indentation: in Python that is an IndentationError or code moved into
    /// another block. Lines inside a multi-line string literal are left
    /// exactly as written (see [`Self::lines_inside_new_strings`]).
    pub(crate) fn replacement_for(
        &self,
        index: usize,
    ) -> Result<std::borrow::Cow<'a, str>, crate::error::AftError> {
        let matched = &self.matches[index];
        let shift = indentation_shift(self.source, self.needle, matched)?;
        if shift == IndentShift::None {
            return Ok(std::borrow::Cow::Borrowed(self.replacement));
        }
        let in_string = self.lines_inside_new_strings(index)?;
        let line_start = self.source[..matched.byte_start]
            .rfind('\n')
            .map_or(0, |at| at + 1);
        let before_match = &self.source[line_start..matched.byte_start];
        let mut shifted =
            String::with_capacity(self.replacement.len() + self.replacement.len() / 4);
        match shift {
            IndentShift::None => unreachable!("handled above"),
            IndentShift::Add(prefix) => {
                // `boundary_match` trims the file's extra leading indentation
                // from the match's start, leaving it in the file just before
                // the match, so the first replacement line already follows it
                // and only the later lines need it. That holds only when
                // exactly the prefix precedes the match, which a blank first
                // line can break.
                if before_match != prefix {
                    return Err(ambiguous_indentation(
                        "its first line is blank, so the indentation of newString's first line is unclear"
                            .to_string(),
                    ));
                }
                for (line, piece) in self.replacement.split_inclusive('\n').enumerate() {
                    if line > 0 && !piece.trim().is_empty() && !in_string[line] {
                        shifted.push_str(&prefix);
                    }
                    shifted.push_str(piece);
                }
            }
            IndentShift::Remove(prefix) => {
                // A deeper oldString makes the match start at the beginning of
                // its first line, so every replacement line loses the prefix.
                if !before_match.is_empty() {
                    return Err(ambiguous_indentation(
                        "its first line is blank, so the indentation of newString's first line is unclear"
                            .to_string(),
                    ));
                }
                for (line, piece) in self.replacement.split_inclusive('\n').enumerate() {
                    if piece.trim().is_empty() || in_string[line] {
                        shifted.push_str(piece);
                        continue;
                    }
                    let Some(rest) = piece.strip_prefix(prefix.as_str()) else {
                        return Err(ambiguous_indentation(format!(
                            "oldString is indented deeper than the file, and line {} of newString does not start with that extra indentation",
                            line + 1
                        )));
                    };
                    shifted.push_str(rest);
                }
            }
        }
        Ok(std::borrow::Cow::Owned(shifted))
    }
}

/// [`ReindentPlan::replacement_for`] for a single match.
#[cfg(test)]
pub(crate) fn reindent_replacement<'r>(
    source: &'r str,
    needle: &'r str,
    matched: &FuzzyMatch,
    replacement: &'r str,
    path: Option<&'r std::path::Path>,
) -> Result<std::borrow::Cow<'r, str>, crate::error::AftError> {
    let matches = std::slice::from_ref(matched);
    let plan = ReindentPlan::new(source, needle, replacement, path, matches);
    plan.replacement_for(0)
        .map(|text| std::borrow::Cow::Owned(text.into_owned()))
}

/// Final pass: windows of whole lines that hold the needle's text with line
/// breaks and spacing moved, as a formatter reflow does. A window matches
/// when its text, with every whitespace run collapsed to one space and the
/// ends trimmed, equals the needle's. One linear scan finds every such
/// window (see [`ReflowCandidates`]), in ascending order of its first line.
fn find_reflow_matches(
    haystack_lines: &[&str],
    needle_lines: &[&str],
    line_offsets: &[usize],
    haystack: &str,
    needle: &str,
) -> Vec<FuzzyMatch> {
    let normalized_needle = normalize_reflow_whitespace(&needle_lines.join("\n"));
    if normalized_needle.is_empty() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for (start, end) in ReflowCandidates::new(haystack_lines, &normalized_needle) {
        let byte_start = line_offsets[start];
        let byte_end = if end < line_offsets.len() {
            line_offsets[end]
        } else {
            haystack.len()
        };
        matches.push(boundary_match(haystack, needle, byte_start, byte_end, 5));
    }

    matches
}

fn normalize_reflow_whitespace(s: &str) -> String {
    #[cfg(test)]
    crate::patch::matcher::REFLOW_WORK.with(|work| work.set(work.get() + s.len()));
    let mut normalized = String::new();
    let mut in_whitespace = false;

    for c in s.trim().chars() {
        if c.is_whitespace() {
            in_whitespace = true;
        } else {
            if in_whitespace && !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.push(c);
            in_whitespace = false;
        }
    }

    normalized
}

#[cfg(test)]
fn strip_reflow_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Normalize Unicode punctuation to ASCII equivalents.
fn normalize_unicode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => '-',
            '\u{00A0}' => ' ',
            _ => c,
        })
        .collect::<String>()
        .replace('\u{2026}', "...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_boundary_preserves_skipped_whitespace_and_separator() {
        let source = "  | For  a task | b |  \n  | For  a task | b |  \n";
        let matches = find_all_fuzzy(source, "| For a task | b |");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].pass, 5);
        let mut result = source.to_string();
        for m in matches.iter().rev() {
            result.replace_range(m.byte_start..m.byte_start + m.byte_len, "| Changed | b |");
        }
        assert_eq!(result, "  | Changed | b |  \n  | Changed | b |  \n");
        assert!(find_all_fuzzy("| a  | b |\n", "a | b |").is_empty());
        assert_eq!(find_all_fuzzy("| a | b |\n", "| a | b |")[0].pass, 1);
    }

    #[test]
    fn replacement_detail_exposes_unicode_difference_but_not_padding() {
        let source = "  let title = “hello”;  \n";
        let needle = "let title = \"hello\";";
        let matches = find_all_fuzzy(source, needle);
        let detail = replacement_detail(source, needle, &matches);
        assert_eq!(detail["pass"], 4);
        assert_eq!(detail["replaced_text"][0], "let title = “hello”;");
        let padded = find_all_fuzzy("  foo  \n", "foo   ");
        assert!(
            replacement_detail("  foo  \n", "foo   ", &padded)["replaced_text"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_exact_match() {
        let matches = find_all_fuzzy("hello world", "world");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].byte_start, 6);
        assert_eq!(matches[0].pass, 1);
    }

    #[test]
    fn test_exact_match_multiple() {
        let matches = find_all_fuzzy("foo bar foo baz foo", "foo");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].byte_start, 0);
        assert_eq!(matches[1].byte_start, 8);
        assert_eq!(matches[2].byte_start, 16);
    }

    #[test]
    fn test_rstrip_match() {
        let source = "  hello  \n  world  \n";
        let needle = "  hello\n  world";
        let matches = find_all_fuzzy(source, needle);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].pass, 2); // rstrip pass
    }

    #[test]
    fn test_trim_match() {
        let source = "    function foo() {\n      return 1;\n    }\n";
        let needle = "function foo() {\n  return 1;\n}";
        let matches = find_all_fuzzy(source, needle);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].pass, 3); // trim pass
    }

    #[test]
    fn test_unicode_normalize() {
        let source = "let msg = \u{201C}hello\u{201D}\n";
        let needle = "let msg = \"hello\"";
        let matches = find_all_fuzzy(source, needle);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].pass, 4); // unicode pass
    }

    #[test]
    fn test_unicode_normalize_multiline_variants() {
        let source = "alpha\n  let title = \u{201C}hello\u{201D}\u{2026}\n  let slug = foo\u{2014}bar\u{00A0}baz\nomega\n";
        let needle = "let title = \"hello\"...\nlet slug = foo-bar baz";
        let matches = find_all_fuzzy(source, needle);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].pass, 4);
        assert_eq!(matches[0].byte_start, source.find("let title").unwrap());
    }

    #[test]
    fn test_no_match() {
        let matches = find_all_fuzzy("hello world", "xyz");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_multiline_exact() {
        let source = "line1\nline2\nline3\nline4\n";
        let needle = "line2\nline3";
        let matches = find_all_fuzzy(source, needle);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].byte_start, 6);
        assert_eq!(matches[0].pass, 1);
    }

    #[test]
    fn test_reflow_one_line_needle_matches_three_line_split() {
        let source = "before\nlet total = alpha +\n    beta +\n    gamma;\nafter\n";
        let needle = "let total = alpha + beta + gamma;";
        let matches = find_all_fuzzy(source, needle);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].pass, 5);
        assert_eq!(matches[0].byte_start, source.find("let total").unwrap());
        assert_eq!(
            &source[matches[0].byte_start..matches[0].byte_start + matches[0].byte_len],
            "let total = alpha +\n    beta +\n    gamma;"
        );
    }

    #[test]
    fn test_reflow_three_line_needle_matches_one_line_join() {
        let source = "before\nlet total = alpha + beta + gamma;\nafter\n";
        let needle = "let total = alpha +\n    beta +\n    gamma;";
        let matches = find_all_fuzzy(source, needle);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].pass, 5);
        assert_eq!(matches[0].byte_start, source.find("let total").unwrap());
        assert_eq!(
            &source[matches[0].byte_start..matches[0].byte_start + matches[0].byte_len],
            "let total = alpha + beta + gamma;"
        );
    }

    #[test]
    fn test_reflow_reports_all_ambiguous_windows() {
        let source =
            "let total = alpha +\n  beta +\n  gamma;\n\nlet total = alpha +\n  beta +\n  gamma;\n";
        let needle = "let total = alpha + beta + gamma;";
        let matches = find_all_fuzzy(source, needle);

        assert_eq!(matches.len(), 2);
        assert!(matches.iter().all(|m| m.pass == 5));
    }

    #[test]
    fn test_reflow_near_miss_does_not_match() {
        let source = "let total = alpha +\n  beta +\n  gamma;\n";
        let needle = "let total = alpha + beta + delta;";
        let matches = find_all_fuzzy(source, needle);

        assert!(matches.is_empty());
    }

    #[test]
    fn test_reflow_does_not_preempt_exact_match() {
        let source = "let total = alpha +\n  beta +\n  gamma;\nlet total = alpha + beta + gamma;\n";
        let needle = "let total = alpha + beta + gamma;";
        let matches = find_all_fuzzy(source, needle);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].pass, 1);
        assert_eq!(matches[0].byte_start, source.rfind("let total").unwrap());
    }

    #[test]
    fn reflow_scan_allocations_do_not_grow_with_file_lines() {
        let line =
            "export const generated = café\u{2003}+\t中 + padding_padding_padding_padding;\n";
        let count = |repeat: usize| {
            let source = line.repeat(repeat);
            let lines: Vec<&str> = source.lines().collect();
            let offsets = compute_line_offsets(&source);
            let needle = "never present";
            let ((), allocations) = crate::test_allocations::count(|| {
                assert!(
                    find_reflow_matches(&lines, &[needle], &offsets, &source, needle).is_empty()
                );
            });
            allocations
        };
        // One stripped byte stream for the whole file, sized up front: the
        // allocations are fixed, never one per line.
        let (small, large) = (count(256), count(4_096));
        assert!(large < 32, "{large} allocations for 4,096 lines");
        assert!(large <= small + 2, "{small} -> {large} allocations");
    }

    const REFLOW_NON_WS_TOLERANCE: usize = 8;

    fn has_reflow_content(s: &str) -> bool {
        s.chars().any(|c| !c.is_whitespace())
    }

    /// The window-extending scan `find_reflow_matches` used before the
    /// linear candidate search, kept unchanged as the reference result.
    fn find_reflow_matches_scan_oracle(
        haystack_lines: &[&str],
        needle_lines: &[&str],
        line_offsets: &[usize],
        haystack: &str,
        needle: &str,
    ) -> Vec<FuzzyMatch> {
        let needle_text = needle_lines.join("\n");
        let normalized_needle = normalize_reflow_whitespace(&needle_text);
        let needle_non_whitespace = strip_reflow_whitespace(&needle_text);
        if normalized_needle.is_empty() || needle_non_whitespace.is_empty() {
            return Vec::new();
        }
        let min_non_whitespace = needle_non_whitespace
            .len()
            .saturating_sub(REFLOW_NON_WS_TOLERANCE);
        let max_non_whitespace = needle_non_whitespace.len() + REFLOW_NON_WS_TOLERANCE;
        let line_non_whitespace_lens: Vec<usize> = haystack_lines
            .iter()
            .map(|line| {
                line.chars()
                    .filter(|character| !character.is_whitespace())
                    .map(char::len_utf8)
                    .sum()
            })
            .collect();
        let mut matches = Vec::new();
        for start in 0..haystack_lines.len() {
            if !has_reflow_content(haystack_lines[start]) {
                continue;
            }
            let mut window_non_whitespace_len = 0usize;
            for end in (start + 1)..=haystack_lines.len() {
                let line = haystack_lines[end - 1];
                window_non_whitespace_len += line_non_whitespace_lens[end - 1];
                if window_non_whitespace_len > max_non_whitespace {
                    break;
                }
                if window_non_whitespace_len < min_non_whitespace {
                    continue;
                }
                if !has_reflow_content(line) {
                    continue;
                }
                let window_text = haystack_lines[start..end].join("\n");
                if strip_reflow_whitespace(&window_text) != needle_non_whitespace {
                    continue;
                }
                if normalize_reflow_whitespace(&window_text) != normalized_needle {
                    continue;
                }
                let byte_start = line_offsets[start];
                let byte_end = if end < line_offsets.len() {
                    line_offsets[end]
                } else {
                    haystack.len()
                };
                matches.push(boundary_match(haystack, needle, byte_start, byte_end, 5));
            }
        }
        matches
    }

    fn spans(matches: &[FuzzyMatch]) -> Vec<(usize, usize, u8)> {
        matches
            .iter()
            .map(|m| (m.byte_start, m.byte_len, m.pass))
            .collect()
    }

    /// `find_line_matches` as it was before the Knuth-Morris-Pratt scan:
    /// every start tried in turn. Kept unchanged as the reference result.
    fn find_line_matches_scan_oracle<F>(
        haystack_lines: &[&str],
        needle_lines: &[&str],
        line_offsets: &[usize],
        haystack: &str,
        needle: &str,
        compare: F,
        pass: u8,
    ) -> Vec<FuzzyMatch>
    where
        F: Fn(&str, &str) -> bool,
    {
        let mut matches = Vec::new();
        if needle_lines.len() > haystack_lines.len() {
            return matches;
        }
        'outer: for i in 0..=(haystack_lines.len() - needle_lines.len()) {
            for j in 0..needle_lines.len() {
                if !compare(haystack_lines[i + j], needle_lines[j]) {
                    continue 'outer;
                }
            }
            let byte_start = line_offsets[i];
            let end_line = i + needle_lines.len();
            let byte_end = if end_line < line_offsets.len() {
                line_offsets[end_line]
            } else {
                haystack.len()
            };
            matches.push(boundary_match(haystack, needle, byte_start, byte_end, pass));
        }
        matches
    }

    /// One line pass (2, 3 or 4) returns the same matches, byte ranges
    /// included, as the old start-by-start scan on generated inputs with
    /// duplicate, empty and whitespace-only lines, CRLF, and repeated blocks.
    fn assert_line_pass_matches_scan(seed: u64, pass: u8) {
        use crate::patch::matcher::line_test_support::{file_lines, pattern_lines};
        use crate::patch::matcher::reflow_test_support::Rng;
        let mut rng = Rng::new(seed);
        let (mut cases_with_matches, mut matches_total) = (0, 0);
        for case in 0..10_000 {
            let lines = file_lines(&mut rng);
            let separator = if rng.below(3) == 0 { "\r\n" } else { "\n" };
            let mut haystack = lines.join(separator);
            if rng.below(2) == 0 {
                haystack.push_str(separator);
            }
            let needle = pattern_lines(&mut rng, &lines).join(separator);
            let haystack_lines: Vec<&str> = haystack.lines().collect();
            let needle_lines: Vec<&str> = needle.lines().collect();
            if needle_lines.is_empty() {
                continue;
            }
            let offsets = compute_line_offsets(&haystack);
            let normalized = |lines: &[&str]| {
                lines
                    .iter()
                    .map(|line| normalize_unicode(line.trim()))
                    .collect::<Vec<_>>()
            };
            let (haystack_normalized, needle_normalized) =
                (normalized(&haystack_lines), normalized(&needle_lines));
            let (keys, needle_keys): (Vec<&str>, Vec<&str>) = if pass == 4 {
                (
                    haystack_normalized.iter().map(String::as_str).collect(),
                    needle_normalized.iter().map(String::as_str).collect(),
                )
            } else {
                (haystack_lines.clone(), needle_lines.clone())
            };
            let compare: fn(&str, &str) -> bool = match pass {
                2 => |a, b| a.trim_end() == b.trim_end(),
                3 => |a, b| a.trim() == b.trim(),
                _ => |a, b| a == b,
            };
            let expected = find_line_matches_scan_oracle(
                &keys,
                &needle_keys,
                &offsets,
                &haystack,
                &needle,
                compare,
                pass,
            );
            let actual = find_line_matches(
                &keys,
                &needle_keys,
                &offsets,
                &haystack,
                &needle,
                compare,
                pass,
            );
            assert_eq!(
                spans(&actual),
                spans(&expected),
                "case {case}: haystack {haystack:?} needle {needle:?}"
            );
            cases_with_matches += usize::from(!actual.is_empty());
            matches_total += actual.len();
        }
        assert!(
            cases_with_matches > 1_000,
            "only {cases_with_matches} cases matched"
        );
        assert!(
            matches_total > cases_with_matches,
            "no case had several matches"
        );
    }

    #[test]
    fn rstrip_pass_matches_the_start_by_start_scan() {
        assert_line_pass_matches_scan(0x11ae_1002, 2);
    }

    #[test]
    fn trim_pass_matches_the_start_by_start_scan() {
        assert_line_pass_matches_scan(0x11ae_1003, 3);
    }

    #[test]
    fn unicode_pass_matches_the_start_by_start_scan() {
        assert_line_pass_matches_scan(0x11ae_1004, 4);
    }

    // BEGIN line-tier work-bound test
    /// Every line of a repetitive file matches the needle's lines except its
    /// last, which differs only in spacing ("ab" for "a b"). Trying every
    /// start compared 7,000 lines (6,999 equal, then the differing last one)
    /// at each of 18,001 starts in each line pass. The three line passes together now stay within a small
    /// multiple of the file plus needle size.
    #[test]
    fn line_pass_work_is_linear_on_repetitive_near_misses() {
        let haystack = "a b\n".repeat(25_000);
        let needle = "a b\n".repeat(6_999) + "ab";
        crate::patch::matcher::LINE_MATCH_WORK.with(|work| work.set(0));
        assert!(find_all_fuzzy(&haystack, &needle).is_empty());
        let work = crate::patch::matcher::LINE_MATCH_WORK.with(std::cell::Cell::get);
        let size = haystack.len() + needle.len();
        assert!(work <= 32 * size, "{work} units of work for {size} bytes");
    }
    // END line-tier work-bound test

    // BEGIN reflow-work-bound test
    /// A repetitive file whose lines all match the needle's text, but the
    /// needle's last line differs only in spacing ("ab" for "a b"). Every
    /// whitespace-stripped occurrence was a spacing near miss that used to be
    /// rescanned in full before being rejected: about 18,000 windows of 28 KB.
    /// The whole pass, building its stream, scanning it and normalizing any
    /// text, stays within a small multiple of the file plus needle size.
    #[test]
    fn reflow_pass_work_is_linear_on_repetitive_spacing_misses() {
        let haystack = "a b\n".repeat(25_000);
        let needle = "a b\n".repeat(6_999) + "ab";
        let haystack_lines: Vec<&str> = haystack.lines().collect();
        let needle_lines: Vec<&str> = needle.lines().collect();
        let offsets = compute_line_offsets(&haystack);
        crate::patch::matcher::REFLOW_WORK.with(|work| work.set(0));
        let matches =
            find_reflow_matches(&haystack_lines, &needle_lines, &offsets, &haystack, &needle);
        let work = crate::patch::matcher::REFLOW_WORK.with(std::cell::Cell::get);
        assert!(matches.is_empty());
        let size = haystack.len() + needle.len();
        assert!(work <= 4 * size, "{work} units of work for {size} bytes");
    }
    // END reflow-work-bound test

    /// The linear reflow pass returns the same matches, byte ranges included,
    /// as the old window scan on generated inputs: whitespace-only lines,
    /// tabs, CRLF, repeated blocks, overlapping candidates, needles a few
    /// characters off, and needles at either end of the file.
    #[test]
    fn linear_reflow_matches_equal_the_window_scan_on_generated_inputs() {
        use crate::patch::matcher::reflow_test_support::{file_lines, needle_text, Rng};
        let mut rng = Rng::new(0x5eed_0002);
        let (mut cases_with_matches, mut matches_total) = (0, 0);
        for case in 0..10_000 {
            let lines = file_lines(&mut rng);
            let separator = if rng.below(3) == 0 { "\r\n" } else { "\n" };
            let mut haystack = lines.join(separator);
            if rng.below(2) == 0 {
                haystack.push_str(separator);
            }
            let mut needle = needle_text(&mut rng, &lines);
            if rng.below(4) == 0 {
                needle = needle.replace('\n', "\r\n");
            }
            let haystack_lines: Vec<&str> = haystack.lines().collect();
            let needle_lines: Vec<&str> = needle.lines().collect();
            let offsets = compute_line_offsets(&haystack);
            let expected = find_reflow_matches_scan_oracle(
                &haystack_lines,
                &needle_lines,
                &offsets,
                &haystack,
                &needle,
            );
            let actual =
                find_reflow_matches(&haystack_lines, &needle_lines, &offsets, &haystack, &needle);
            assert_eq!(
                spans(&actual),
                spans(&expected),
                "case {case}: haystack {haystack:?} needle {needle:?}"
            );
            cases_with_matches += usize::from(!actual.is_empty());
            matches_total += actual.len();
        }
        assert!(
            cases_with_matches > 300,
            "only {cases_with_matches} cases matched"
        );
        assert!(
            matches_total > cases_with_matches,
            "no case had several matches"
        );
    }

    /// A large reflowed `match` against a large file is found by the reflow
    /// pass, as before, and quickly.
    #[test]
    fn large_reflowed_needle_still_matches() {
        let lines = (0..6_000)
            .map(|index| format!("    call_{index}(argument_{index});"))
            .collect::<Vec<_>>();
        let source = lines.join("\n") + "\n";
        let needle = lines[1_000..3_000]
            .chunks(2)
            .map(|pair| {
                pair.iter()
                    .map(|line| line.trim())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let matches = find_all_fuzzy(&source, &needle);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].pass, 5);
        assert_eq!(
            &source[matches[0].byte_start..matches[0].byte_start + matches[0].byte_len],
            lines[1_000..3_000].join("\n").trim_start()
        );
    }

    #[test]
    fn nearest_miss_renders_candidate_and_first_divergence() {
        let source =
            "fn calculate() {\n    let first = 1;\n    let actual = 2;\n    let last = 3;\n}\n";
        let needle =
            "fn calculate() {\n    let first = 1;\n    let expected = 2;\n    let last = 3;\n}";
        let detail = render_nearest_miss_detail(source, needle);

        assert!(detail.contains("Nearest candidate at lines 1-5"));
        assert!(detail.contains("- expected:     let expected = 2;"));
        assert!(detail.contains("+ actual:     let actual = 2;"));
    }

    #[test]
    fn nearest_miss_below_floor_reports_only_line_count() {
        let source = "function totallyDifferent() {\n    return 42;\n}\n";
        let needle = "function target() {\n    return expected;\n}";
        let detail = render_nearest_miss_detail(source, needle);

        assert_eq!(detail, " (file has 3 lines)");
    }

    #[test]
    fn nearest_miss_excerpt_middle_truncates_after_ten_lines() {
        let source = (1..=12)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let needle = (1..=12)
            .map(|line| {
                if line == 6 {
                    "different line".to_string()
                } else {
                    format!("line {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let detail = render_nearest_miss_detail(&source, &needle);

        assert!(detail.contains("middle candidate lines truncated"));
        assert!(detail.contains("line 1"));
        assert!(detail.contains("line 12"));
        assert!(!detail.contains("line 6 |"));
    }

    #[test]
    fn occurrence_listing_is_one_based_and_capped() {
        let source = "same\n".repeat(10);
        let positions = source
            .match_indices("same")
            .map(|(offset, _)| offset)
            .collect::<Vec<_>>();
        let detail = render_occurrence_listing(&source, &positions);

        assert!(detail.starts_with(" 10 occurrences: #1 at line 1, #2 at line 2"));
        assert!(detail.contains("#8 at line 8"));
        assert!(detail.contains("… and 2 more"));
        assert!(!detail.contains("#9 at line 9"));
    }
}

#[cfg(test)]
mod audit_regressions {
    #[test]
    fn whitespace_match_has_valid_range() {
        for source in ["a\n \nb\n", "a\n   \nb\n", "a\n \u{2003} \nb\n"] {
            let matches = super::find_all_fuzzy(source, "\t");
            assert!(!matches.is_empty());
            for m in matches {
                assert!(source
                    .get(m.byte_start..m.byte_start + m.byte_len)
                    .is_some());
            }
        }
    }
}

#[cfg(test)]
mod edit_not_found_detail_tests {
    use super::render_edit_not_found_detail;

    #[test]
    fn already_applied_hint_reports_one_line_or_repeated_occurrences() {
        let once =
            render_edit_not_found_detail("first\nreplacement\nlast\n", "missing", "replacement");
        assert!(once
            .contains("newString is already present at line 2; the edit may already be applied."));

        let repeated = render_edit_not_found_detail(
            "replacement\nother replacement\n",
            "missing",
            "replacement",
        );
        assert!(repeated.contains("newString occurs 2 times; the edit may already be applied."));
        assert!(!repeated.contains("newString is already present at line"));
    }

    #[test]
    fn already_applied_hint_skips_empty_whitespace_and_old_string_substrings() {
        for (old_string, new_string) in [
            ("missing", ""),
            ("missing", " \t\n "),
            ("missing replacement", "replacement"),
            ("missing", "not present"),
        ] {
            let detail = render_edit_not_found_detail("replacement\n", old_string, new_string);
            assert!(
                !detail.contains("newString"),
                "unexpected hint for newString {new_string:?}: {detail}"
            );
        }
    }
}

#[cfg(test)]
mod indentation_shift_tests {
    use super::*;

    /// Pins every notice the reply can show, word for word.
    #[test]
    fn loose_match_notice_full_strings() {
        let expected = [
            (2, false, "oldString matched only after ignoring trailing whitespace."),
            (3, false, "oldString matched only after ignoring indentation and trailing whitespace."),
            (3, true, "oldString matched only after ignoring indentation and trailing whitespace; newString was re-indented to match the file."),
            (4, false, "oldString matched only after ignoring indentation, trailing whitespace and Unicode punctuation (curly quotes, dashes, ellipses, non-breaking spaces)."),
            (4, true, "oldString matched only after ignoring indentation, trailing whitespace and Unicode punctuation (curly quotes, dashes, ellipses, non-breaking spaces); newString was re-indented to match the file."),
            (5, false, "oldString matched only after ignoring line breaks and spacing."),
        ];
        for (pass, reindented, notice) in expected {
            assert_eq!(
                loose_match_notice(pass, reindented).as_deref(),
                Some(notice),
                "pass {pass}, reindented {reindented}"
            );
        }
        assert_eq!(loose_match_notice(1, false), None);
    }

    #[test]
    fn replacement_detail_carries_the_notice_and_shift() {
        let needle = "if x:\n    a = 1\n    b = 2";
        let matches = find_all_fuzzy(PY, needle);
        let detail = replacement_detail(PY, needle, &matches);
        assert_eq!(detail["reindented"], true);
        assert_eq!(
            detail["notice"],
            "oldString matched only after ignoring indentation and trailing whitespace; newString was re-indented to match the file."
        );
    }

    /// Re-indent `replacement` for the only match of `needle` in `source`,
    /// asserting the match came from `pass`.
    fn reindent(source: &str, needle: &str, replacement: &str, pass: u8) -> Result<String, String> {
        reindent_in(None, source, needle, replacement, pass)
    }

    /// [`reindent`] for a file at `path`, whose language decides which lines
    /// are inside multi-line strings. `None` is text without strings.
    fn reindent_in(
        path: Option<&str>,
        source: &str,
        needle: &str,
        replacement: &str,
        pass: u8,
    ) -> Result<String, String> {
        let matches = find_all_fuzzy(source, needle);
        assert_eq!(matches.len(), 1, "{source:?} / {needle:?}");
        assert_eq!(matches[0].pass, pass, "{source:?} / {needle:?}");
        reindent_replacement(
            source,
            needle,
            &matches[0],
            replacement,
            path.map(std::path::Path::new),
        )
        .map(|text| text.into_owned())
        .map_err(|error| {
            assert_eq!(error.code(), "ambiguous_indentation");
            error.to_string()
        })
    }

    const PY: &str = "def f(x):\n    if x:\n        a = 1\n        b = 2\n    return x\n";

    #[test]
    fn shallow_needle_shifts_every_later_replacement_line() {
        // The first line already follows the file's prefix, which stays
        // outside the match; every later non-blank line gets the prefix.
        assert_eq!(
            reindent(
                PY,
                "if x:\n    a = 1\n    b = 2",
                "if x:\n    a = 10\n\n    b = 20",
                3
            )
            .unwrap(),
            "if x:\n        a = 10\n\n        b = 20"
        );
        // A one-line needle off by the same prefix shifts added lines too.
        assert_eq!(
            reindent(PY, "a = 1  ", "a = 1\nc = 3", 3).unwrap(),
            "a = 1\n        c = 3"
        );
    }

    #[test]
    fn deep_needle_drops_the_extra_prefix_from_every_line() {
        let source = "def f():\n    a = 1\n    b = 2\n";
        assert_eq!(
            reindent(
                source,
                "        a = 1\n        b = 2",
                "        a = 10\n        if a:\n            b = 3\n",
                3
            )
            .unwrap(),
            "    a = 10\n    if a:\n        b = 3\n"
        );
    }

    #[test]
    fn tab_indented_file_shifts_by_tabs_and_keeps_crlf() {
        let source = "def f(x):\r\n\tif x:\r\n\t\ta = 1\r\n";
        assert_eq!(
            reindent(source, "if x:\r\n\ta = 1", "if x:\r\n\ta = 2\r\n\tb = 3", 3).unwrap(),
            "if x:\r\n\t\ta = 2\r\n\t\tb = 3"
        );
    }

    #[test]
    fn unicode_pass_shifts_like_the_trim_pass() {
        let source = "    if x:\n        title = \u{201c}a\u{201d}\n";
        assert_eq!(
            reindent(
                source,
                "if x:\n    title = \"a\"",
                "if x:\n    title = \"b\"",
                4
            )
            .unwrap(),
            "if x:\n        title = \"b\""
        );
    }

    #[test]
    fn passes_that_keep_or_lose_line_pairing_never_shift() {
        // Pass 2 compares leading whitespace exactly.
        assert_eq!(
            reindent("    a = 1   \n", "    a = 1\t", "    a = 2\nb", 2).unwrap(),
            "    a = 2\nb"
        );
        // Pass 5 has no line pairing to measure.
        let reflow = "    call(a,\n         b)\n";
        assert_eq!(
            reindent(reflow, "call(a, b)", "call(a,\nb)", 5).unwrap(),
            "call(a,\nb)"
        );
    }

    #[test]
    fn tabs_against_spaces_are_refused() {
        let source = "\tif x:\n\t\ta = 1\n";
        assert_eq!(
            reindent(source, "if x:\n    a = 1", "if x:\n    a = 2", 3).unwrap_err(),
            "oldString matched only after ignoring indentation, but line 2 of the file is indented with different whitespace (tabs versus spaces) than oldString, so newString cannot be re-indented without guessing. Nothing was changed; copy oldString with the file's exact indentation."
        );
    }

    #[test]
    fn different_offsets_on_different_lines_are_refused() {
        // oldString was flattened: the file is 2 deeper on line 1 but 4
        // deeper on line 2.
        let source = "  if (x) {\n    a();\n  }\n";
        assert_eq!(
            reindent(source, "if (x) {\na();\n}", "if (x) {\nb();\n}", 3).unwrap_err(),
            "oldString matched only after ignoring indentation, but its indentation differs from the file's by different amounts on different lines (first at line 2), so newString cannot be re-indented without guessing. Nothing was changed; copy oldString with the file's exact indentation."
        );
    }

    #[test]
    fn deep_needle_with_shallower_replacement_line_is_refused() {
        let source = "def f():\n    a = 1\n";
        let error = reindent(source, "        a = 1", "        a = 2\n  b = 3", 3).unwrap_err();
        assert!(
            error.contains("line 2 of newString does not start with that extra indentation"),
            "{error}"
        );
    }

    const PY_PAIR: &str = "def f():\n    a = 1\n    b = 2\n";

    #[test]
    fn lines_inside_a_new_multiline_string_are_not_shifted() {
        // Shallow oldString: code lines get four spaces, the string's
        // interior and closing quotes keep the agent's text.
        assert_eq!(
            reindent_in(
                Some("t.py"),
                PY_PAIR,
                "a = 1\nb = 2",
                "a = \"\"\"\nhello\n  world\n\"\"\"\nb = 2",
                3
            )
            .unwrap(),
            "a = \"\"\"\nhello\n  world\n\"\"\"\n    b = 2"
        );
        // Deep oldString: the extra depth is removed from code lines only.
        assert_eq!(
            reindent_in(
                Some("t.py"),
                PY_PAIR,
                "        a = 1\n        b = 2",
                "        a = '''\n        kept\n        '''\n        b = 2",
                3
            )
            .unwrap(),
            "    a = '''\n        kept\n        '''\n    b = 2"
        );
        // An implicit concatenation across lines is code between strings,
        // so its continuation line moves with the block.
        assert_eq!(
            reindent_in(
                Some("t.py"),
                PY_PAIR,
                "a = 1\nb = 2",
                "a = (\"x\"\n     \"y\")\nb = 2",
                3
            )
            .unwrap(),
            "a = (\"x\"\n         \"y\")\n    b = 2"
        );
    }

    #[test]
    fn match_inside_an_existing_multiline_string_is_refused() {
        let source = "s = \"\"\"\n    one\n    two\n\"\"\"\n";
        let error =
            reindent_in(Some("t.py"), source, "one\ntwo", "one\ntwo\nthree", 3).unwrap_err();
        assert_eq!(
            error,
            "oldString matched only after ignoring indentation, but line 2 of newString is inside a multi-line string that starts before the edit, and that string's intended text is unclear, so newString cannot be re-indented without guessing. Nothing was changed; copy oldString with the file's exact indentation."
        );
    }

    #[test]
    fn unparsed_or_unknown_text_with_string_delimiters_is_refused() {
        // A file type with no parser: a backtick could open a multi-line
        // string, so no shift is safe.
        let source = "block {\n    a\n    b\n}\n";
        let error =
            reindent_in(Some("t.unknownext"), source, "a\nb", "a\n`x\ny`\nb", 3).unwrap_err();
        assert!(error.contains("no parser here"), "{error}");
        // Without delimiters it shifts as usual.
        assert_eq!(
            reindent_in(Some("t.unknownext"), source, "a\nb", "a\nc\nb", 3).unwrap(),
            "a\n    c\n    b"
        );
        // A parse broken around the edit while quotes are present.
        let ts = "function f() {\n  const a = 1;\n  return a;\n}\n";
        let error = reindent_in(
            Some("t.ts"),
            ts,
            "const a = 1;\nreturn a;",
            "const a = (1;\nconst s = 'x';\nreturn a;",
            3,
        )
        .unwrap_err();
        assert!(error.contains("does not parse"), "{error}");
        // Text without string literals (a GitHub comment) shifts every line.
        assert_eq!(
            reindent(
                "- item\n  - a\n  - b\n",
                "- a\n- b",
                "- a\n```\ncode\n```",
                3
            )
            .unwrap(),
            "- a\n  ```\n  code\n  ```"
        );
    }

    #[test]
    fn text_lines_inside_strings_marks_string_interiors_only() {
        let path = std::path::Path::new("t.py");
        let lang = crate::parser::LangId::Python;
        let body = "    \"\"\"Doc\n    more\n    \"\"\"\n    x = (\"a\"\n         \"b\")\n";
        assert_eq!(
            text_lines_inside_strings(body, path, lang),
            Some(vec![false, true, true, false, false])
        );
        // Text without a string delimiter is answered without parsing.
        STRING_LINE_PARSES.with(|count| count.set(0));
        assert_eq!(
            text_lines_inside_strings("a = 1\nb = 2\n", path, lang),
            Some(vec![false, false])
        );
        assert_eq!(STRING_LINE_PARSES.with(|count| count.get()), 0);
        // A broken parse with quotes present gives no answer.
        assert_eq!(
            text_lines_inside_strings("x = ('a'\ny = )\n", path, lang),
            None
        );
    }
}
