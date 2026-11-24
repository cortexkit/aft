use crate::compress::{CompressionResult, Compressor};

pub fn strip_ansi(input: &str) -> String {
    let stripped = strip_ansi_cow(input);
    #[cfg(any(test, feature = "test-support"))]
    if matches!(stripped, std::borrow::Cow::Borrowed(_)) {
        ANSI_STRIP_ALLOC_BYTES.with(|bytes| bytes.set(bytes.get() + input.len()));
    }
    stripped.into_owned()
}

/// Borrow plain text; ANSI-bearing input still uses the exact same stripping
/// grammar. Dispatch and its output probe can share this single derived view.
pub(super) fn strip_ansi_cow(input: &str) -> std::borrow::Cow<'_, str> {
    record_ansi_strip();
    let bytes = input.as_bytes();
    let Some(mut index) = bytes.iter().position(|byte| *byte == 0x1b) else {
        return std::borrow::Cow::Borrowed(input);
    };
    let mut output = String::with_capacity(input.len());
    #[cfg(any(test, feature = "test-support"))]
    ANSI_STRIP_ALLOC_BYTES.with(|bytes| bytes.set(bytes.get() + output.capacity()));
    let mut last_kept = 0;

    while index < bytes.len() {
        if bytes[index] != 0x1b {
            index += 1;
            continue;
        }

        let Some(next) = bytes.get(index + 1).copied() else {
            break;
        };

        let end = if next == b'[' {
            let mut cursor = index + 2;
            while cursor < bytes.len() {
                if (0x40..=0x7e).contains(&bytes[cursor]) {
                    cursor += 1;
                    break;
                }
                cursor += 1;
            }
            cursor
        } else if matches!(next, b']' | b'P' | b'X' | b'^' | b'_') {
            control_string_end(bytes, index + 2)
        } else if (0x40..=0x5f).contains(&next) {
            index + 2
        } else if let Some(end) = designator_end(bytes, index + 1) {
            end
        } else {
            index += 1;
            continue;
        };

        output.push_str(&input[last_kept..index]);
        index = end.min(bytes.len());
        last_kept = index;
    }

    output.push_str(&input[last_kept..]);
    std::borrow::Cow::Owned(output)
}

/// End of an OSC, DCS, SOS, PM or APC string whose payload starts at `start`:
/// just past its BEL or `ESC \` terminator. A string still open at a newline
/// ends there, leaving the newline, so one malformed sequence cannot swallow
/// the rest of the output.
fn control_string_end(bytes: &[u8], start: usize) -> usize {
    let mut cursor = start;
    while cursor < bytes.len() {
        match bytes[cursor] {
            0x07 => return cursor + 1,
            0x1b if bytes.get(cursor + 1) == Some(&b'\\') => return cursor + 2,
            b'\n' => return cursor,
            _ => cursor += 1,
        }
    }
    cursor
}

/// End of an escape such as the charset designator `ESC ( B`: intermediate
/// bytes (0x20-0x2f) starting at `start`, then one final byte (0x30-0x7e).
fn designator_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start;
    while bytes.get(cursor).is_some_and(|byte| (0x20..=0x2f).contains(byte)) {
        cursor += 1;
    }
    if cursor == start {
        return None;
    }
    bytes
        .get(cursor)
        .is_some_and(|byte| (0x30..=0x7e).contains(byte))
        .then_some(cursor + 1)
}

pub fn dedup_consecutive(input: &str) -> String {
    let had_trailing_newline = input.ends_with('\n');
    let mut output = String::with_capacity(input.len());
    let mut lines = input.lines();

    let Some(mut current) = lines.next() else {
        return String::new();
    };
    let mut count = 1usize;

    for line in lines {
        if line == current {
            count += 1;
        } else {
            push_dedup_run(&mut output, current, count);
            current = line;
            count = 1;
        }
    }
    push_dedup_run(&mut output, current, count);

    if !had_trailing_newline {
        output.pop();
    }

    output
}

fn push_dedup_run(output: &mut String, line: &str, count: usize) {
    output.push_str(line);
    output.push('\n');
    if count >= 4 {
        output.push_str("... (");
        output.push_str(&(count - 1).to_string());
        output.push_str(" more)\n");
    } else {
        for _ in 1..count {
            output.push_str(line);
            output.push('\n');
        }
    }
}

pub fn middle_truncate(
    input: &str,
    threshold_bytes: usize,
    keep_head: usize,
    keep_tail: usize,
) -> String {
    if input.len() <= threshold_bytes {
        return input.to_string();
    }

    let head_end = floor_char_boundary(input, keep_head.min(input.len()));
    let tail_start = ceil_char_boundary(input, input.len().saturating_sub(keep_tail));

    if head_end >= tail_start {
        return input.to_string();
    }

    let truncated_bytes = tail_start - head_end;
    let mut output = String::with_capacity(head_end + keep_tail + 64);
    output.push_str(&input[..head_end]);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str("...<truncated ");
    output.push_str(&truncated_bytes.to_string());
    output.push_str(" bytes>...\n");
    output.push_str(&input[tail_start..]);
    output
}

pub fn floor_char_boundary(input: &str, mut index: usize) -> usize {
    while index > 0 && !input.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub fn ceil_char_boundary(input: &str, mut index: usize) -> usize {
    while index < input.len() && !input.is_char_boundary(index) {
        index += 1;
    }
    index
}

pub struct GenericCompressor;

impl GenericCompressor {
    pub fn compress_output(output: &str) -> String {
        let stripped = strip_ansi(output);
        Self::compress_stripped_output(&stripped)
    }

    pub(crate) fn compress_stripped_output(output: &str) -> String {
        dedup_consecutive(output)
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static ANSI_STRIP_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ANSI_STRIP_ALLOC_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "test-support"))]
fn record_ansi_strip() {
    ANSI_STRIP_COUNT.with(|count| count.set(count.get() + 1));
}

#[cfg(all(debug_assertions, any(test, feature = "test-support")))]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn reset_ansi_strip_alloc_bytes() {
    ANSI_STRIP_ALLOC_BYTES.with(|bytes| bytes.set(0));
}

#[cfg(all(debug_assertions, any(test, feature = "test-support")))]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn ansi_strip_alloc_bytes() -> usize {
    ANSI_STRIP_ALLOC_BYTES.with(std::cell::Cell::get)
}

#[cfg(not(any(test, feature = "test-support")))]
fn record_ansi_strip() {}

#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn reset_ansi_strip_count() {
    ANSI_STRIP_COUNT.with(|count| count.set(0));
}

#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn ansi_strip_count() -> usize {
    ANSI_STRIP_COUNT.with(std::cell::Cell::get)
}

impl Compressor for GenericCompressor {
    fn matches(&self, _command: &str) -> bool {
        true
    }

    fn compress_with_exit_code(
        &self,
        _command: &str,
        output: &str,
        _exit_code: Option<i64>,
    ) -> CompressionResult {
        Self::compress_output(output).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_does_not_pretruncate_above_old_five_kib_threshold() {
        let input = (0..900)
            .map(|idx| format!("unique-line-{idx:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(input.len() > 5 * 1024);

        let compressed = GenericCompressor::compress_output(&input);

        assert!(!compressed.contains("...<truncated "));
        assert!(compressed.len() > 5 * 1024);
        assert!(compressed.contains("unique-line-0000"));
        assert!(compressed.contains("unique-line-0899"));
    }

    #[test]
    fn strip_ansi_removes_osc_strings_and_charset_designators() {
        // Window title ended by BEL, and OSC 8 hyperlinks ended by ESC \.
        assert_eq!(strip_ansi("\x1b]0;my window title\x07hello"), "hello");
        assert_eq!(
            strip_ansi("see \x1b]8;;https://example.com/doc\x1b\\the docs\x1b]8;;\x1b\\ now"),
            "see the docs now"
        );
        // `tput sgr0` on xterm-like terminals emits the designator ESC ( B.
        assert_eq!(strip_ansi("\x1b[1mbold\x1b(B\x1b[m plain"), "bold plain");
        // A device control string is removed up to its ESC \ terminator.
        assert_eq!(strip_ansi("a\x1bPq#0;2\x1b\\b"), "ab");
    }

    #[test]
    fn strip_ansi_keeps_text_after_an_unterminated_osc_line() {
        // A string that never terminates ends at its line, so later output is
        // not swallowed.
        assert_eq!(strip_ansi("\x1b]0;title\nnext line"), "\nnext line");
    }
}
