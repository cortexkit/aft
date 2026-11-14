//! Shared line cut for compressors that must shorten output by line count.
//!
//! Scripts, test runners and builds print their verdict last: the summary, the
//! final error, "N passed; M failed", the exit reason. A cap that keeps only the
//! first lines drops exactly that verdict, so every line cut in the compressors
//! goes through here and keeps a tail window as well as the head. The omitted
//! lines are replaced by one marker line in the middle that names how many were
//! left out. The marker is metadata, not output: the bash `shown N of M lines`
//! trailer skips it (see [`is_omitted_lines_marker`]), so `shown` counts exactly
//! the head and tail lines that survived.

/// Most lines the tail window keeps. Twenty lines covers a test runner's
/// summary block or a build's final error with a little context.
pub const TAIL_WINDOW_LINES: usize = 20;

/// Most bytes the tail window keeps, so a few huge final lines cannot crowd out
/// the head. The final line itself is exempt: it is always kept whole.
pub const TAIL_WINDOW_BYTES: usize = 4 * 1024;

/// Most input lines [`ensure_final_lines`] appends when a summary extractor's
/// result does not end with the input's last line (for example a wrapper
/// script's verdict printed after the cargo output the extractor recognised).
pub const FINAL_LINES_KEEP: usize = 5;

/// Byte budget for the lines added by [`ensure_final_lines`]; the very last
/// line is kept whatever its size.
pub const FINAL_LINES_BYTES: usize = 1024;

/// How many leading and trailing lines a cut keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPlan {
    pub head: usize,
    pub tail: usize,
}

/// Decide how to show at most `max_lines` of `line_count` lines.
///
/// Returns `None` when everything fits. Otherwise `head + tail == max_lines`.
/// The tail window takes up to [`TAIL_WINDOW_LINES`] lines (never more than
/// half of `max_lines`, so a head-biased cap stays head-biased) and up to
/// [`TAIL_WINDOW_BYTES`] bytes as measured by `line_len`. The final line is
/// always in the tail when `max_lines > 0`, even when it alone is larger than
/// the byte budget: the last line wins over head lines.
pub fn plan_head_tail(
    line_count: usize,
    max_lines: usize,
    line_len: impl Fn(usize) -> usize,
) -> Option<CutPlan> {
    if line_count <= max_lines {
        return None;
    }
    if max_lines == 0 {
        return Some(CutPlan { head: 0, tail: 0 });
    }
    let tail_limit = TAIL_WINDOW_LINES.min((max_lines / 2).max(1));
    let mut tail = 1;
    let mut bytes = line_len(line_count - 1);
    while tail < tail_limit {
        let next = line_len(line_count - 1 - tail) + 1;
        if bytes + next > TAIL_WINDOW_BYTES {
            break;
        }
        bytes += next;
        tail += 1;
    }
    Some(CutPlan {
        head: max_lines - tail,
        tail,
    })
}

/// Marker line standing in for `omitted` cut lines. `noun` names what was cut,
/// for example `"lines"` or `"push lines"`.
pub fn omitted_marker(omitted: usize, noun: &str) -> String {
    let noun = if omitted == 1 {
        noun.strip_suffix('s').unwrap_or(noun)
    } else {
        noun
    };
    format!("... {omitted} {noun} omitted ...")
}

/// True for a line produced by [`omitted_marker`]. Line counters use this to
/// keep the marker out of the number of lines shown.
pub fn is_omitted_lines_marker(line: &str) -> bool {
    let Some(rest) = line
        .trim()
        .strip_prefix("... ")
        .and_then(|rest| rest.strip_suffix(" omitted ..."))
    else {
        return false;
    };
    let Some((count, noun)) = rest.split_once(' ') else {
        return false;
    };
    !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()) && !noun.trim().is_empty()
}

/// Keep at most `max_lines` of `lines`: the head, a marker naming the omitted
/// count, then the tail window. Returns the lines unchanged when they fit.
pub fn cap_lines_head_tail<S: AsRef<str>>(
    lines: &[S],
    max_lines: usize,
    noun: &str,
) -> Vec<String> {
    let Some(plan) = plan_head_tail(lines.len(), max_lines, |index| lines[index].as_ref().len())
    else {
        return lines.iter().map(|line| line.as_ref().to_string()).collect();
    };
    render_cut(lines, plan, lines.len() - plan.head - plan.tail, noun)
}

/// Assemble the head, the omitted marker and the tail for a planned cut.
///
/// `lines` may be a compacted view whose middle was never stored (a streaming
/// cap); `omitted` is the true number of lines left out, which the caller
/// knows even when `lines` does not hold them all.
pub fn render_cut<S: AsRef<str>>(
    lines: &[S],
    plan: CutPlan,
    omitted: usize,
    noun: &str,
) -> Vec<String> {
    let mut kept = Vec::with_capacity(plan.head + plan.tail + 1);
    kept.extend(
        lines
            .iter()
            .take(plan.head)
            .map(|line| line.as_ref().to_string()),
    );
    if omitted > 0 && plan.head + plan.tail > 0 {
        kept.push(omitted_marker(omitted, noun));
    }
    kept.extend(
        lines[lines.len() - plan.tail..]
            .iter()
            .map(|line| line.as_ref().to_string()),
    );
    kept
}

/// Text form of [`cap_lines_head_tail`]. Trailing whitespace is trimmed and
/// trailing blank lines do not count, so the tail window ends at the last line
/// that says something.
pub fn cap_text_head_tail(input: &str, max_lines: usize) -> String {
    let trimmed = input.trim_end();
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() <= max_lines {
        return trimmed.to_string();
    }
    cap_lines_head_tail(&lines, max_lines, "lines").join("\n")
}

/// Preserve the input's final non-empty line and a short, byte-bounded window
/// before it. Only that window is retained; a reverse prefix scan computes the
/// omitted-line marker when the compressed ending occurred earlier in the input.
pub fn ensure_final_lines(input: &str, compressed: &str) -> String {
    let mut earlier = input.lines().map(str::trim_end).rev().peekable();
    let Some(last) = earlier.find(|line| !line.is_empty()) else {
        return compressed.to_string();
    };
    let compressed_last = compressed
        .lines()
        .map(str::trim_end)
        .rfind(|line| !line.is_empty());
    if compressed_last == Some(last) {
        return compressed.to_string();
    }
    let mut window = Vec::with_capacity(FINAL_LINES_KEEP);
    window.push(last);
    let mut bytes = last.len();
    while window.len() < FINAL_LINES_KEEP {
        let Some(next) = earlier.peek() else { break };
        if bytes + next.len() + 1 > FINAL_LINES_BYTES {
            break;
        }
        bytes += next.len() + 1;
        window.push(earlier.next().unwrap());
    }
    window.reverse();
    #[cfg(any(test, feature = "test-support"))]
    FINAL_LINE_REFS.with(|refs| refs.set(window.len()));
    let mut append_from = 0;
    let mut gap = None;
    if let Some(compressed_last) = compressed_last.filter(|line| !is_omitted_lines_marker(line)) {
        if let Some(offset) = window.iter().rposition(|line| *line == compressed_last) {
            append_from = offset + 1;
            gap = Some(0);
        } else {
            gap = earlier.position(|line| line == compressed_last);
        }
    }
    let mut output = compressed.trim_end().to_string();
    if let Some(omitted) = gap.filter(|omitted| *omitted > 0) {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&omitted_marker(omitted, "lines"));
    }
    for line in &window[append_from..] {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(line);
    }
    output
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static FINAL_LINE_REFS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a02_final_line_window_has_bounded_storage() {
        let input = "earlier\n".repeat(100_000) + "GATE PASSED\n\n";
        let result = ensure_final_lines(&input, "earlier");
        assert!(result.ends_with("GATE PASSED"));
        assert!(
            FINAL_LINE_REFS.with(|refs| refs.get()) <= FINAL_LINES_KEEP,
            "final-line repair retained the entire input line vector"
        );
    }

    fn numbered(count: usize) -> Vec<String> {
        (1..=count).map(|index| format!("line {index}")).collect()
    }

    #[test]
    fn fitting_lines_are_returned_unchanged() {
        let lines = numbered(5);
        assert_eq!(cap_lines_head_tail(&lines, 5, "lines"), lines);
        assert_eq!(plan_head_tail(5, 5, |_| 1), None);
    }

    #[test]
    fn cut_keeps_head_marker_and_tail_window() {
        let lines = numbered(283);
        let kept = cap_lines_head_tail(&lines, 79, "lines");
        let plan = plan_head_tail(283, 79, |index| lines[index].len()).expect("cut");
        assert_eq!(plan, CutPlan { head: 59, tail: 20 });
        assert_eq!(kept.len(), 80, "79 shown lines plus one marker");
        assert_eq!(kept[0], "line 1");
        assert_eq!(kept[58], "line 59");
        assert_eq!(kept[59], "... 204 lines omitted ...");
        assert_eq!(kept[60], "line 264");
        assert_eq!(kept.last().map(String::as_str), Some("line 283"));
        let shown = kept
            .iter()
            .filter(|line| !is_omitted_lines_marker(line))
            .count();
        assert_eq!(
            shown + 204,
            283,
            "shown plus omitted accounts for every line"
        );
    }

    #[test]
    fn small_caps_keep_the_tail_window_to_half() {
        let lines = numbered(10);
        assert_eq!(
            cap_lines_head_tail(&lines, 2, "lines"),
            vec!["line 1", "... 8 lines omitted ...", "line 10"]
        );
        assert_eq!(
            cap_lines_head_tail(&lines, 1, "lines"),
            vec!["... 9 lines omitted ...", "line 10"]
        );
        assert!(cap_lines_head_tail(&lines, 0, "lines").is_empty());
    }

    #[test]
    fn oversized_final_line_wins_over_the_byte_budget() {
        let mut lines = numbered(100);
        let huge = "x".repeat(TAIL_WINDOW_BYTES * 2);
        lines.push(huge.clone());
        let kept = cap_lines_head_tail(&lines, 50, "lines");
        assert_eq!(kept.last(), Some(&huge), "final line is never cut");
        assert_eq!(kept.len(), 51);
        assert_eq!(kept[49], "... 51 lines omitted ...");
    }

    #[test]
    fn tail_window_respects_the_byte_budget() {
        let lines: Vec<String> = (0..100).map(|index| format!("{index:0>1000}")).collect();
        let plan = plan_head_tail(lines.len(), 60, |index| lines[index].len()).expect("cut");
        // 1000-byte lines plus newlines: four fit in 4 KiB, a fifth does not.
        assert_eq!(plan, CutPlan { head: 56, tail: 4 });
    }

    #[test]
    fn marker_detection_is_exact() {
        assert!(is_omitted_lines_marker("... 12 lines omitted ..."));
        assert!(is_omitted_lines_marker("... 1 push line omitted ..."));
        assert!(!is_omitted_lines_marker("... lines omitted ..."));
        assert!(!is_omitted_lines_marker("... 12x lines omitted ..."));
        assert!(!is_omitted_lines_marker("12 lines omitted"));
        assert_eq!(omitted_marker(1, "lines"), "... 1 line omitted ...");
    }

    #[test]
    fn text_cut_ignores_trailing_blank_lines() {
        let input = format!("{}\nVERDICT\n\n\n", numbered(30).join("\n"));
        let capped = cap_text_head_tail(&input, 10);
        assert!(capped.ends_with("VERDICT"), "{capped}");
        assert_eq!(cap_text_head_tail("a\nb\n\n", 5), "a\nb");
    }

    #[test]
    fn final_lines_are_appended_with_gap_marker() {
        let input = "running 2 tests\ntest result: ok. 2 passed\nsummary\nother 1\nother 2\nother 3\nother 4\nGATE PASSED\n";
        let compressed = "running 2 tests\ntest result: ok. 2 passed";
        assert_eq!(
            ensure_final_lines(input, compressed),
            "running 2 tests\ntest result: ok. 2 passed\n... 1 line omitted ...\nother 1\nother 2\nother 3\nother 4\nGATE PASSED"
        );
    }

    #[test]
    fn final_lines_inside_window_are_not_repeated() {
        let input = "a\nb\ntest result: ok\nwrapper done\n";
        assert_eq!(
            ensure_final_lines(input, "test result: ok"),
            "test result: ok\nwrapper done"
        );
        assert_eq!(
            ensure_final_lines(input, "test result: ok\nwrapper done"),
            "test result: ok\nwrapper done"
        );
        assert_eq!(ensure_final_lines("\n\n", "x"), "x");
    }
}
