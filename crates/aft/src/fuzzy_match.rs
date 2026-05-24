//! Fuzzy string matching for edit_match, inspired by opencode's 4-pass approach.
//!
//! When exact matching fails, progressively relaxes comparison:
//!   Pass 1: Exact match (str::find / match_indices)
//!   Pass 2: Trim trailing whitespace per line
//!   Pass 3: Trim both ends per line
//!   Pass 4: Normalize Unicode punctuation + trim

/// A match result: byte offset in source and the matched byte length.
#[derive(Debug, Clone)]
pub struct FuzzyMatch {
    pub byte_start: usize,
    pub byte_len: usize,
    /// Which pass found the match (1=exact, 2=rstrip, 3=trim, 4=unicode)
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
        |a, b| a.trim() == b.trim(),
        3,
    );
    if !trim_matches.is_empty() {
        return trim_matches;
    }

    // Pass 4: normalized Unicode + trim
    let normalized_matches = find_line_matches(
        &haystack_lines,
        &needle_lines,
        &line_byte_offsets,
        haystack,
        |a, b| normalize_unicode(a.trim()) == normalize_unicode(b.trim()),
        4,
    );
    normalized_matches
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
fn find_line_matches<F>(
    haystack_lines: &[&str],
    needle_lines: &[&str],
    line_offsets: &[usize],
    haystack: &str,
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
        // Found a match at line `i` spanning `needle_lines.len()` lines
        let byte_start = line_offsets[i];
        let end_line = i + needle_lines.len();
        let byte_end = if end_line < line_offsets.len() {
            // Include the newline after the last matched line
            line_offsets[end_line]
        } else {
            haystack.len()
        };
        matches.push(FuzzyMatch {
            byte_start,
            byte_len: byte_end - byte_start,
            pass,
        });
    }

    matches
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
}
