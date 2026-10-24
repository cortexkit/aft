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
/// non-whitespace content different from the requested text.
pub fn replacement_detail(source: &str, needle: &str, matches: &[FuzzyMatch]) -> serde_json::Value {
    let pass = matches.first().map(|m| m.pass).unwrap_or(1);
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
    serde_json::json!({"pass": pass, "replaced_text": actual})
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
