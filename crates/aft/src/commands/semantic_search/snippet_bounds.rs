//! Size bounds for the text `aft_search` prints.
//!
//! Search results carry display snippets read from the matched files. A
//! minified bundle or a captured JSON document can keep its whole content on a
//! single line, so printing "the matching line" of such a file used to print
//! hundreds of kilobytes for one hit. The bounds here are applied where the
//! text is produced, not by cutting a finished reply:
//! - every snippet line is cut to the grep tool's line limit, with the grep
//!   tool's truncation marker;
//! - when the match sits far into a long line, the kept part of the line is a
//!   window around the match, so the reader still sees why the file matched;
//! - one reply stops adding result text once it reaches the grep tool's page
//!   budget, and says so.

use std::borrow::Cow;

use crate::commands::grep::{
    truncate_grep_line, GREP_LINE_TRUNCATED_MARKER, GREP_MAX_LINE_CHARS, GREP_MAX_OUTPUT_BYTES,
};

/// Longest snippet line, in characters, a search reply prints.
pub(crate) const SNIPPET_LINE_MAX_CHARS: usize = GREP_MAX_LINE_CHARS;

/// Byte budget for the result text of one search reply (paths, symbol headers
/// and snippets). Result text past the budget is not rendered.
pub(crate) const SEARCH_MAX_OUTPUT_BYTES: usize = GREP_MAX_OUTPUT_BYTES;

/// Appended to a snippet line that was cut before its end.
pub(crate) const SNIPPET_LINE_TRUNCATED_MARKER: &str = GREP_LINE_TRUNCATED_MARKER;

/// Printed in place of the text skipped before a window that starts mid-line.
const LEADING_ELLIPSIS: &str = "…";

/// Characters kept ahead of the match when a window starts mid-line, so the
/// match is shown with some of the text that leads into it.
const WINDOW_LEAD_CHARS: usize = 80;

/// Cut a snippet line to [`SNIPPET_LINE_MAX_CHARS`] characters from its start.
///
/// A line this module already cut (it ends with the truncation marker and is
/// within the limit before it) is returned unchanged, so applying the cap at
/// both the snippet source and the renderer never cuts a line twice.
pub(crate) fn cap_snippet_line(line: &str) -> Cow<'_, str> {
    // A line of at most SNIPPET_LINE_MAX_CHARS bytes cannot hold more
    // characters than that, so the common short line skips the char count.
    if line.len() <= SNIPPET_LINE_MAX_CHARS || is_already_bounded(line) {
        return Cow::Borrowed(line);
    }
    match truncate_grep_line(line) {
        (text, true) => Cow::Owned(text),
        (_, false) => Cow::Borrowed(line),
    }
}

/// Owned variant of [`cap_snippet_line`] that reuses the line's allocation
/// when nothing is cut.
pub(crate) fn cap_owned_snippet_line(line: String) -> String {
    match cap_snippet_line(&line) {
        Cow::Borrowed(_) => line,
        Cow::Owned(capped) => capped,
    }
}

fn is_already_bounded(line: &str) -> bool {
    line.strip_suffix(SNIPPET_LINE_TRUNCATED_MARKER)
        .is_some_and(|kept| kept.chars().count() <= SNIPPET_LINE_MAX_CHARS)
}

/// Bound `line` to [`SNIPPET_LINE_MAX_CHARS`] characters while keeping the text
/// at byte offset `anchor` (the start of the match) visible.
///
/// A line within the limit is returned whole. A match near the start keeps the
/// start of the line. A match further in yields a window that starts a little
/// before the match, prefixed with an ellipsis and, when the line continues
/// past the window, followed by the truncation marker.
pub(crate) fn window_snippet_line(line: &str, anchor: usize) -> String {
    if line.len() <= SNIPPET_LINE_MAX_CHARS {
        return line.to_string();
    }
    let total_chars = line.chars().count();
    if total_chars <= SNIPPET_LINE_MAX_CHARS {
        return line.to_string();
    }
    let anchor = floor_char_boundary(line, anchor);
    let chars_before_anchor = line[..anchor].chars().count();
    if chars_before_anchor <= WINDOW_LEAD_CHARS {
        return cap_snippet_line(line).into_owned();
    }

    // One character of the limit goes to the leading ellipsis. When the match
    // is near the end of the line, the window starts earlier so it stays full.
    let body_chars = SNIPPET_LINE_MAX_CHARS - LEADING_ELLIPSIS.chars().count();
    let start_char =
        (chars_before_anchor - WINDOW_LEAD_CHARS).min(total_chars.saturating_sub(body_chars));
    let start_byte = line
        .char_indices()
        .nth(start_char)
        .map_or(line.len(), |(index, _)| index);
    let rest = &line[start_byte..];
    match rest.char_indices().nth(body_chars) {
        None => format!("{LEADING_ELLIPSIS}{rest}"),
        Some((cut, _)) => format!(
            "{LEADING_ELLIPSIS}{}{SNIPPET_LINE_TRUNCATED_MARKER}",
            &rest[..cut]
        ),
    }
}

/// Byte offset of the first case-insensitive (ASCII) occurrence of `needle`
/// in `line`, preferring an occurrence that stands as a whole word (not inside
/// a longer identifier) over one embedded in a longer word. `needle` must
/// already be lowercase.
pub(crate) fn find_ascii_case_insensitive(line: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    // ASCII lowercasing keeps every byte offset, so an offset found in the
    // lowered copy is valid in `line`.
    let lowered = line.to_ascii_lowercase();
    let bytes = lowered.as_bytes();
    let is_word_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut first = None;
    for (start, _) in lowered.match_indices(needle) {
        first.get_or_insert(start);
        let end = start + needle.len();
        let starts_word = start == 0 || !is_word_byte(bytes[start - 1]);
        let ends_word = end == bytes.len() || !is_word_byte(bytes[end]);
        if starts_word && ends_word {
            return Some(start);
        }
    }
    first
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Result text under construction for one reply, bounded by a byte budget.
///
/// Pieces are appended in reading order. The first piece is always accepted
/// (a reply never comes back empty while results exist); after that, the first
/// piece that would pass the budget is refused, and so is every later piece,
/// so no text is rendered past the cut.
pub(crate) struct BudgetedText {
    text: String,
    max_bytes: usize,
    cut: bool,
}

impl BudgetedText {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            max_bytes,
            cut: false,
        }
    }

    /// Append `piece` and return true, or refuse it (and everything after it)
    /// and return false when it would pass the budget.
    pub(crate) fn push(&mut self, piece: &str) -> bool {
        if self.cut {
            return false;
        }
        if !self.text.is_empty() && self.text.len() + piece.len() > self.max_bytes {
            self.cut = true;
            return false;
        }
        self.text.push_str(piece);
        true
    }

    pub(crate) fn is_cut(&self) -> bool {
        self.cut
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn into_string(self) -> String {
        self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_lines_are_untouched() {
        assert_eq!(cap_snippet_line("fn main() {}"), "fn main() {}");
        assert_eq!(window_snippet_line("fn main() {}", 3), "fn main() {}");
    }

    #[test]
    fn long_line_is_cut_at_the_grep_limit_with_the_grep_marker() {
        let line = "é".repeat(SNIPPET_LINE_MAX_CHARS + 50);
        let capped = cap_snippet_line(&line);
        assert_eq!(
            capped,
            format!(
                "{}{}",
                "é".repeat(SNIPPET_LINE_MAX_CHARS),
                SNIPPET_LINE_TRUNCATED_MARKER
            )
        );
        // Capping an already capped line changes nothing.
        assert_eq!(cap_snippet_line(&capped), capped);
    }

    #[test]
    fn window_keeps_a_match_deep_inside_a_long_line() {
        let line = format!("{}needle_here{}", "a".repeat(10_000), "b".repeat(10_000));
        let anchor = line.find("needle_here").unwrap();
        let window = window_snippet_line(&line, anchor);
        assert!(window.starts_with(LEADING_ELLIPSIS));
        assert!(window.ends_with(SNIPPET_LINE_TRUNCATED_MARKER));
        assert!(window.contains("needle_here"));
        let body = window.strip_suffix(SNIPPET_LINE_TRUNCATED_MARKER).unwrap();
        assert_eq!(body.chars().count(), SNIPPET_LINE_MAX_CHARS);
        assert_eq!(cap_snippet_line(&window), window);
    }

    #[test]
    fn window_near_the_end_of_a_line_stays_full_and_unmarked() {
        let line = format!("{}needle", "a".repeat(2_000));
        let window = window_snippet_line(&line, line.len() - "needle".len());
        assert!(window.starts_with(LEADING_ELLIPSIS));
        assert!(window.ends_with("needle"));
        assert_eq!(window.chars().count(), SNIPPET_LINE_MAX_CHARS);
    }

    #[test]
    fn window_with_an_early_match_keeps_the_line_start() {
        let line = format!("needle{}", "a".repeat(2_000));
        let window = window_snippet_line(&line, 0);
        assert!(window.starts_with("needle"));
        assert!(window.ends_with(SNIPPET_LINE_TRUNCATED_MARKER));
    }

    #[test]
    fn anchor_prefers_a_whole_word_occurrence() {
        assert_eq!(
            find_ascii_case_insensitive("prefix then Fix it", "fix"),
            Some(12)
        );
        assert_eq!(find_ascii_case_insensitive("prefix only", "fix"), Some(3));
        assert_eq!(find_ascii_case_insensitive("nothing", "fix"), None);
    }

    #[test]
    fn budget_accepts_the_first_piece_and_refuses_everything_after_the_cut() {
        let mut text = BudgetedText::new(10);
        assert!(text.push("0123456789abc"));
        assert!(!text.push("x"));
        assert!(text.is_cut());
        assert!(!text.push(""));
        assert_eq!(text.into_string(), "0123456789abc");
    }
}
