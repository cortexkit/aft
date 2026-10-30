use crate::compress::generic::{ceil_char_boundary, floor_char_boundary};

pub const FINAL_OUTPUT_CAP_BYTES: usize = 16 * 1024;
pub const FINAL_OUTPUT_HEAD_BYTES: usize = 6 * 1024;
pub const FINAL_OUTPUT_TAIL_BYTES: usize = 10 * 1024;

pub const RUNNING_OUTPUT_PREVIEW_BYTES: usize = 8 * 1024;

// Completion previews ride inside reminder/wake messages and in-turn tool
// result appends — they are a glance, not the output channel (full output is
// always recoverable via bash_status / the stdout+stderr file pointers). They
// are sized by exit status:
//  - success: a short tail is all the agent needs ("done, here's the gist");
//    head context is dead weight at ~150 tokens/task.
//  - failure: keep a small head (the command banner / first error) plus a
//    meaningful tail (tracebacks and test summaries land at the end).
// The uniform 4 KiB head+tail this replaced injected ~1K tokens of mostly
// build noise per completed task into reminders.
pub const COMPLETION_SUCCESS_PREVIEW_BYTES: usize = 600;
pub const COMPLETION_SUCCESS_HEAD_BYTES: usize = 0;
pub const COMPLETION_SUCCESS_TAIL_BYTES: usize = 600;
pub const COMPLETION_FAILURE_PREVIEW_BYTES: usize = 2304;
pub const COMPLETION_FAILURE_HEAD_BYTES: usize = 512;
pub const COMPLETION_FAILURE_TAIL_BYTES: usize = 1792;

pub const RAW_PASSTHROUGH_CAP_BYTES: usize = 50 * 1024;
pub const RAW_PASSTHROUGH_HEAD_BYTES: usize = 20 * 1024;
pub const RAW_PASSTHROUGH_TAIL_BYTES: usize = 30 * 1024;

pub const STRUCTURED_OUTPUT_CAP_BYTES: usize = 50 * 1024;

pub const COMPRESS_INPUT_CAP_BYTES: usize = 10 * 1024 * 1024;
pub const COMPRESS_INPUT_HEAD_BYTES: usize = 4 * 1024 * 1024;
pub const COMPRESS_INPUT_TAIL_BYTES: usize = 6 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CappedText {
    pub text: String,
    pub truncated: bool,
}

/// Where one run of a capped text came from: copied unchanged from a byte range
/// of the input, or written by the cap itself (a truncation marker, a recovery
/// marker, a summary line, or a separating newline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CapPiece {
    Input(std::ops::Range<usize>),
    Inserted(String),
}

/// A capped text together with the provenance of every byte in it. Completion
/// previews mask AFT's ticket by its position in the source, so they need to
/// know which input bytes a displayed line was copied from, not just its text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PiecedText {
    pub(crate) text: String,
    pub(crate) pieces: Vec<CapPiece>,
}

impl PiecedText {
    pub(crate) fn whole(input: &str) -> Self {
        let mut text = Self::default();
        text.copy(input, 0..input.len());
        text
    }

    pub(crate) fn inserted(text: &str) -> Self {
        let mut pieced = Self::default();
        pieced.insert(text);
        pieced
    }

    pub(crate) fn copy(&mut self, input: &str, range: std::ops::Range<usize>) {
        self.text.push_str(&input[range.clone()]);
        push_piece(&mut self.pieces, CapPiece::Input(range));
    }

    pub(crate) fn insert(&mut self, text: &str) {
        self.text.push_str(text);
        push_piece(&mut self.pieces, CapPiece::Inserted(text.to_string()));
    }

    /// Append another pieced text whose input ranges refer to the same input.
    fn append(&mut self, other: PiecedText) {
        self.text.push_str(&other.text);
        for piece in other.pieces {
            push_piece(&mut self.pieces, piece);
        }
    }

    /// Keep only the first `len` bytes (a char boundary of `text`).
    fn truncate(&mut self, len: usize) {
        self.text.truncate(len);
        let mut kept = 0;
        let mut pieces = Vec::new();
        for piece in std::mem::take(&mut self.pieces) {
            if kept >= len {
                break;
            }
            let room = len - kept;
            let piece = match piece {
                CapPiece::Input(range) if range.len() > room => {
                    CapPiece::Input(range.start..range.start + room)
                }
                CapPiece::Inserted(text) if text.len() > room => {
                    CapPiece::Inserted(text[..room].to_string())
                }
                piece => piece,
            };
            kept += piece_len(&piece);
            pieces.push(piece);
        }
        self.pieces = pieces;
    }

    /// `self` was capped from `inner.text`. Re-express its copied ranges in
    /// terms of `inner`'s own input, so they point at the original bytes.
    pub(crate) fn through(self, inner: &PiecedText) -> PiecedText {
        let mut pieces = Vec::new();
        for piece in self.pieces {
            let range = match piece {
                CapPiece::Input(range) => range,
                inserted => {
                    push_piece(&mut pieces, inserted);
                    continue;
                }
            };
            let mut offset = 0;
            for inner_piece in &inner.pieces {
                let len = piece_len(inner_piece);
                let from = range.start.max(offset);
                let to = range.end.min(offset + len);
                if from < to {
                    let local = from - offset..to - offset;
                    push_piece(
                        &mut pieces,
                        match inner_piece {
                            CapPiece::Input(source) => CapPiece::Input(
                                source.start + local.start..source.start + local.end,
                            ),
                            CapPiece::Inserted(text) => CapPiece::Inserted(text[local].to_string()),
                        },
                    );
                }
                offset += len;
            }
        }
        PiecedText {
            text: self.text,
            pieces,
        }
    }

    fn into_capped(self, truncated: bool) -> CappedText {
        CappedText {
            text: self.text,
            truncated,
        }
    }
}

fn piece_len(piece: &CapPiece) -> usize {
    match piece {
        CapPiece::Input(range) => range.len(),
        CapPiece::Inserted(text) => text.len(),
    }
}

fn push_piece(pieces: &mut Vec<CapPiece>, piece: CapPiece) {
    if piece_len(&piece) == 0 {
        return;
    }
    match (pieces.last_mut(), piece) {
        (Some(CapPiece::Input(last)), CapPiece::Input(range)) if last.end == range.start => {
            last.end = range.end;
        }
        (Some(CapPiece::Inserted(last)), CapPiece::Inserted(text)) => last.push_str(&text),
        (_, piece) => pieces.push(piece),
    }
}

/// Byte offset of `line` inside `input`; `line` must be a subslice of it.
fn offset_in(input: &str, line: &str) -> usize {
    line.as_ptr() as usize - input.as_ptr() as usize
}

pub fn cap_final_output(input: &str) -> CappedText {
    cap_final_output_preserving(input, None, &[], false)
}

pub fn cap_final_output_with_marker(input: &str, marker: &str) -> CappedText {
    cap_final_output_preserving(input, Some(marker), &[], false)
}

/// A compressed reply must retain the ending of each capture, not just the
/// ending of their concatenation. Reserve those lines before ordinary context.
/// `failed` (a known non-zero exit) also keeps failure lines from the cut
/// middle of the output.
pub(crate) fn cap_final_output_preserving(
    input: &str,
    marker: Option<&str>,
    final_lines: &[String],
    failed: bool,
) -> CappedText {
    let (pieced, truncated) = cap_preserving_pieces(
        input,
        FINAL_OUTPUT_CAP_BYTES,
        FINAL_OUTPUT_HEAD_BYTES,
        FINAL_OUTPUT_TAIL_BYTES,
        marker,
        final_lines,
        failed,
    );
    pieced.into_capped(truncated)
}

fn cap_preserving_pieces(
    input: &str,
    threshold: usize,
    keep_head: usize,
    keep_tail: usize,
    marker: Option<&str>,
    final_lines: &[String],
    rescue_failures: bool,
) -> (PiecedText, bool) {
    cap_test_verdict(
        input,
        threshold,
        keep_head,
        keep_tail,
        marker,
        final_lines,
        rescue_failures,
    )
    .map(|pieced| (pieced, true))
    .unwrap_or_else(|| match marker {
        Some(marker) => {
            cap_head_tail_with_marker_pieces(input, threshold, keep_head, keep_tail, marker)
        }
        None => cap_head_tail_pieces(input, threshold, keep_head, keep_tail),
    })
}

/// Byte threshold under which a completion preview is passed through uncapped.
pub fn completion_preview_threshold(exit_ok: bool) -> usize {
    completion_caps(exit_ok).0
}

fn completion_caps(exit_ok: bool) -> (usize, usize, usize) {
    if exit_ok {
        (
            COMPLETION_SUCCESS_PREVIEW_BYTES,
            COMPLETION_SUCCESS_HEAD_BYTES,
            COMPLETION_SUCCESS_TAIL_BYTES,
        )
    } else {
        (
            COMPLETION_FAILURE_PREVIEW_BYTES,
            COMPLETION_FAILURE_HEAD_BYTES,
            COMPLETION_FAILURE_TAIL_BYTES,
        )
    }
}

/// Cap a completion preview by exit status: success keeps a short tail,
/// failure keeps a small head plus a larger tail (see the constants above).
pub fn cap_completion_output(input: &str, exit_ok: bool) -> CappedText {
    cap_completion_output_preserving(input, None, exit_ok, &[])
}

pub fn cap_completion_output_with_marker(input: &str, marker: &str, exit_ok: bool) -> CappedText {
    cap_completion_output_preserving(input, Some(marker), exit_ok, &[])
}

pub(crate) fn cap_completion_output_preserving(
    input: &str,
    marker: Option<&str>,
    exit_ok: bool,
    final_lines: &[String],
) -> CappedText {
    let (pieced, truncated) =
        cap_completion_output_preserving_pieces(input, marker, exit_ok, final_lines);
    pieced.into_capped(truncated)
}

/// [`cap_completion_output_preserving`] that also reports which input bytes
/// each run of the capped text was copied from.
pub(crate) fn cap_completion_output_preserving_pieces(
    input: &str,
    marker: Option<&str>,
    exit_ok: bool,
    final_lines: &[String],
) -> (PiecedText, bool) {
    let (threshold, head, tail) = completion_caps(exit_ok);
    // Completion previews are a short glance; full replies keep failure lines.
    cap_preserving_pieces(input, threshold, head, tail, marker, final_lines, false)
}

/// Reserve the runner's verdict before spending bytes on ordinary head/tail
/// context. Cargo writes test results to stdout and progress/rerun instructions
/// to stderr; concatenating those streams puts the results in the *middle*.
/// A chronological byte cut can therefore hide every failure in a failed run.
///
/// The byte cap is soft when mandatory names/totals or final command lines
/// themselves exceed it: the verdict takes precedence over the preview budget.
fn cap_test_verdict(
    input: &str,
    threshold: usize,
    keep_head: usize,
    keep_tail: usize,
    marker: Option<&str>,
    final_lines: &[String],
    rescue_failures: bool,
) -> Option<PiecedText> {
    if input.len() + marker.map_or(0, |marker| marker.len() + 1) <= threshold {
        return None;
    }
    let lines: Vec<&str> = input.trim_end().lines().collect();
    let verdict = crate::compress::cargo::test_verdict(&lines);
    // Even a non-runner command's last line is never split by a compressed
    // byte cap. Ordinary fitting tails retain their original cap policy.
    let last = lines.len().checked_sub(1)?;
    if !rescue_failures
        && verdict.is_none()
        && final_lines.is_empty()
        && lines[last].len() <= keep_tail
    {
        return None;
    }
    let mut required = vec![false; lines.len()];
    let mut empty = vec![false; lines.len()];
    if let Some(verdict) = verdict {
        for index in verdict.required {
            required[index] = true;
        }
        for index in verdict.empty_results {
            empty[index] = true;
        }
    }
    for final_line in final_lines {
        let Some(index) = lines.iter().rposition(|line| *line == final_line) else {
            continue;
        };
        // Empty test targets already have an equivalent counted summary.
        // Do not turn the last empty target of stdout into a duplicate total.
        // Only the latest occurrence needs protection: a repeated "ok" must
        // not make every earlier "ok" mandatory and defeat the byte budget.
        if !empty[index] {
            required[index] = true;
        }
    }
    // Like the compressor line cut, always retain the command's last line,
    // including a wrapper's verdict after the runner's own summary.
    required[last] = true;
    let mut empty_count = 0;
    for (index, is_empty) in empty.iter_mut().enumerate() {
        if *is_empty && !required[index] {
            empty_count += 1;
        } else {
            *is_empty = false;
        }
    }
    // Each kept line is copied from the input; the empty-target summary and
    // the joining newlines are written by the cap.
    let mut context = PiecedText::default();
    let mut essentials = PiecedText::default();
    let (mut context_lines, mut essential_lines) = (0, 0);
    let mut counted_empty = false;
    for (index, line) in lines.iter().enumerate() {
        let line_range = offset_in(input, line)..offset_in(input, line) + line.len();
        if required[index] || empty[index] {
            if empty[index] && counted_empty {
                continue;
            }
            if essential_lines > 0 {
                essentials.insert("\n");
            }
            essential_lines += 1;
            if required[index] {
                essentials.copy(input, line_range);
            } else {
                essentials.insert(&format!("test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out ({empty_count} empty test targets)"));
                counted_empty = true;
            }
        } else {
            if context_lines > 0 {
                context.insert("\n");
            }
            context_lines += 1;
            context.copy(input, line_range);
        }
    }
    let marker = marker.unwrap_or("[truncated output; full output unavailable]");
    let available = threshold.saturating_sub(essentials.text.len() + marker.len() + 2);
    // Keep the familiar head plus up to twenty final context lines, bounded by
    // the compressor tail window. Mandatory verdict lines do not spend that
    // window, and the actual last command line is already in essentials.
    let mut tail_bytes = 0;
    let mut tail_lines = 0;
    for line in context.text.lines().rev() {
        let bytes = line.len() + 1;
        if tail_lines == crate::compress::line_cut::TAIL_WINDOW_LINES
            || tail_bytes + bytes > keep_tail.min(crate::compress::line_cut::TAIL_WINDOW_BYTES)
        {
            break;
        }
        tail_bytes += bytes;
        tail_lines += 1;
    }
    let cap_context = |reserve: usize| {
        let available = available.saturating_sub(reserve);
        cap_head_tail_with_marker_pieces(
            &context.text,
            available + marker.len() + 1,
            keep_head.min(available),
            tail_bytes.min(available),
            marker,
        )
        .0
    };
    let mut capped = cap_context(0);
    // A failed command's error can sit anywhere in a long output; the head and
    // tail windows above keep neither the middle nor its failure lines. Keep
    // up to RESCUED_FAILURE_LINES of them from the cut. The block's bytes come
    // out of the head budget, which changes what the cut drops, so pick the
    // lines again from the new cut; the third pass reserves the block's
    // largest possible size, so it always fits.
    let mut rescued = PiecedText::default();
    if rescue_failures {
        let mut reserve = 0;
        for attempt in 0..3 {
            rescued = rescued_failure_lines(&context.text, &capped);
            let needed = if rescued.text.is_empty() {
                0
            } else {
                rescued.text.len() + 1
            };
            if needed <= reserve {
                break;
            }
            reserve = if attempt == 0 {
                needed
            } else {
                RESCUED_FAILURE_BLOCK_MAX_BYTES
            };
            capped = cap_context(reserve);
        }
    }
    let mut capped = capped.through(&context);
    capped.truncate(capped.text.trim_end().len());
    capped.insert("\n");
    if !rescued.text.is_empty() {
        capped.append(rescued.through(&context));
        capped.insert("\n");
    }
    capped.append(essentials);
    Some(capped)
}

/// Most failure lines a failed command's reply keeps from the cut middle of
/// its output, and the most bytes those lines may take together.
const RESCUED_FAILURE_LINES: usize = 20;
const RESCUED_FAILURE_BYTES: usize = crate::compress::line_cut::TAIL_WINDOW_BYTES;
/// The rescued block at its largest: label, lines and their newlines, plus
/// the newline that joins the block to what follows it.
const RESCUED_FAILURE_BLOCK_MAX_BYTES: usize = crate::compress::PRESERVED_FAILURES_LABEL.len()
    + RESCUED_FAILURE_BYTES
    + RESCUED_FAILURE_LINES
    + 1;

#[cfg(test)]
thread_local! {
    static RESCUE_LINES_EXAMINED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The labelled block of failure lines that `capped` (cut from `context`)
/// dropped, in output order, in `context` coordinates. Empty when none was
/// dropped. Scanning stops at the line limit; a line larger than the bytes
/// left is skipped. A line already shown, or rescued once, is not repeated.
fn rescued_failure_lines(context: &str, capped: &PiecedText) -> PiecedText {
    let mut gaps = Vec::new();
    let mut kept_to = 0;
    for piece in &capped.pieces {
        if let CapPiece::Input(range) = piece {
            if range.start > kept_to {
                gaps.push(kept_to..range.start);
            }
            kept_to = kept_to.max(range.end);
        }
    }
    if kept_to < context.len() {
        gaps.push(kept_to..context.len());
    }
    let mut block = PiecedText::default();
    let mut shown: Option<std::collections::HashSet<&str>> = None;
    let mut seen = std::collections::HashSet::new();
    let mut lower = String::new();
    let (mut lines, mut bytes) = (0, 0);
    'gaps: for gap in gaps {
        // Only whole lines: a gap that starts inside a line skips its rest.
        let mut start = gap.start;
        if start > 0 && context.as_bytes()[start - 1] != b'\n' {
            match context[start..gap.end].find('\n') {
                Some(at) => start += at + 1,
                None => continue,
            }
        }
        while start < gap.end {
            let end = context[start..]
                .find('\n')
                .map_or(context.len(), |at| start + at);
            if end > gap.end {
                break;
            }
            #[cfg(test)]
            RESCUE_LINES_EXAMINED.with(|count| count.set(count.get() + 1));
            let line = context[start..end].trim_end_matches('\r');
            let line_start = start;
            start = end + 1;
            let trimmed = line.trim();
            if trimmed.is_empty() || bytes + line.len() > RESCUED_FAILURE_BYTES {
                continue;
            }
            lower.clear();
            lower.push_str(trimmed);
            lower.make_ascii_lowercase();
            if !crate::compress::line_has_failure_signal_lower(trimmed, &lower) {
                continue;
            }
            let shown = shown.get_or_insert_with(|| capped.text.lines().map(str::trim).collect());
            if shown.contains(trimmed) || !seen.insert(trimmed) {
                continue;
            }
            if block.text.is_empty() {
                block.insert(crate::compress::PRESERVED_FAILURES_LABEL);
            }
            block.insert("\n");
            block.copy(context, line_start..line_start + line.len());
            lines += 1;
            bytes += line.len();
            if lines == RESCUED_FAILURE_LINES {
                break 'gaps;
            }
        }
    }
    block
}

pub fn cap_head_tail(
    input: &str,
    threshold_bytes: usize,
    keep_head: usize,
    keep_tail: usize,
) -> CappedText {
    let (pieced, truncated) = cap_head_tail_pieces(input, threshold_bytes, keep_head, keep_tail);
    pieced.into_capped(truncated)
}

fn cap_head_tail_pieces(
    input: &str,
    threshold_bytes: usize,
    keep_head: usize,
    keep_tail: usize,
) -> (PiecedText, bool) {
    if input.len() <= threshold_bytes {
        return (PiecedText::whole(input), false);
    }

    let head_end = floor_char_boundary(input, keep_head.min(input.len()));
    let mut tail_start = ceil_char_boundary(input, input.len().saturating_sub(keep_tail));

    if head_end >= tail_start {
        return (PiecedText::whole(input), false);
    }

    let marker_prefix_len = if head_end == 0 || input[..head_end].ends_with('\n') {
        0
    } else {
        1
    };
    loop {
        let truncated_bytes = tail_start - head_end;
        let marker_len = marker_prefix_len
            + "...<truncated ".len()
            + truncated_bytes.to_string().len()
            + " bytes>...\n".len();
        let max_tail = threshold_bytes.saturating_sub(head_end + marker_len);
        let adjusted_tail_start = ceil_char_boundary(input, input.len().saturating_sub(max_tail));
        if adjusted_tail_start <= tail_start {
            break;
        }
        tail_start = adjusted_tail_start;
        if head_end >= tail_start {
            return (PiecedText::whole(input), false);
        }
    }

    let truncated_bytes = tail_start - head_end;
    let mut output = PiecedText::default();
    output.copy(input, 0..head_end);
    // Only insert a separator newline when there IS a head that doesn't end
    // with one — mirroring marker_prefix_len above. With keep_head == 0 the
    // old `!output.ends_with('\n')` check pushed an unbudgeted leading
    // newline (empty string never ends with '\n'), overshooting the
    // threshold by one byte.
    if head_end > 0 && !output.text.ends_with('\n') {
        output.insert("\n");
    }
    output.insert(&format!("...<truncated {truncated_bytes} bytes>...\n"));
    output.copy(input, tail_start..input.len());

    (output, true)
}

pub fn cap_head_tail_with_marker(
    input: &str,
    threshold_bytes: usize,
    keep_head: usize,
    keep_tail: usize,
    marker: &str,
) -> CappedText {
    let (pieced, truncated) =
        cap_head_tail_with_marker_pieces(input, threshold_bytes, keep_head, keep_tail, marker);
    pieced.into_capped(truncated)
}

fn cap_head_tail_with_marker_pieces(
    input: &str,
    threshold_bytes: usize,
    keep_head: usize,
    keep_tail: usize,
    marker: &str,
) -> (PiecedText, bool) {
    if marker.is_empty() {
        return cap_head_tail_pieces(input, threshold_bytes, keep_head, keep_tail);
    }
    if input.len() <= threshold_bytes {
        let with_marker = append_marker_line(input, marker);
        if with_marker.text.len() <= threshold_bytes {
            return (with_marker, true);
        }
    }

    let mut head_budget = keep_head.min(input.len());
    let mut tail_budget = keep_tail.min(input.len());
    let mut seen_budgets = Vec::new();

    for _ in 0..8 {
        let head_end = floor_char_boundary(input, head_budget);
        let marker_prefix_len = if head_end == 0 || input[..head_end].ends_with('\n') {
            0
        } else {
            1
        };
        let marker_len = marker_prefix_len + marker.len() + 1;
        let available = threshold_bytes.saturating_sub(marker_len);
        let next_head = keep_head.min(available).min(input.len());
        let next_tail = keep_tail
            .min(available.saturating_sub(next_head))
            .min(input.len().saturating_sub(next_head));

        if next_head == head_budget && next_tail == tail_budget {
            break;
        }
        if seen_budgets.contains(&(next_head, next_tail)) {
            if next_head.saturating_add(next_tail) < head_budget.saturating_add(tail_budget) {
                head_budget = next_head;
                tail_budget = next_tail;
            }
            break;
        }
        seen_budgets.push((head_budget, tail_budget));
        head_budget = next_head;
        tail_budget = next_tail;
    }

    let head_end = floor_char_boundary(input, head_budget);
    let tail_start = ceil_char_boundary(input, input.len().saturating_sub(tail_budget));
    let tail_start = if head_end >= tail_start {
        input.len()
    } else {
        tail_start
    };
    let (head_end, tail_start) = snap_cut_to_lines(
        input,
        head_end,
        tail_start,
        threshold_bytes.saturating_sub(marker.len() + 1),
    );

    (
        marker_capped_output(input, head_end, tail_start, marker, threshold_bytes),
        true,
    )
}

/// Move a head/tail byte cut onto line boundaries.
///
/// The bash `shown N of M lines` trailer counts the lines around the cut, so a
/// cut through the middle of a line would show a fragment and count it as a
/// line. The head therefore ends after its last complete line and the tail
/// starts at its first complete line; both moves only shrink the kept text.
/// One exception: when the byte cut lands inside the final line, the tail is
/// widened to that whole line (the verdict of most commands) and the head gives
/// up bytes to pay for it, as long as the final line fits `budget` (the bytes
/// available for head plus tail). A final line larger than that is still cut.
fn snap_cut_to_lines(
    input: &str,
    head_end: usize,
    tail_start: usize,
    budget: usize,
) -> (usize, usize) {
    let line_start_at_or_before = |index: usize| input[..index].rfind('\n').map_or(0, |at| at + 1);
    let bytes = input.as_bytes();
    let mut head = if head_end == 0 || bytes[head_end - 1] == b'\n' {
        head_end
    } else {
        line_start_at_or_before(head_end)
    };
    if tail_start >= input.len() {
        return (head, tail_start);
    }

    let content_end = input.trim_end_matches(['\n', '\r']).len();
    let final_line_start = line_start_at_or_before(content_end);
    let mut tail = tail_start;
    if tail_start > 0 && bytes[tail_start - 1] != b'\n' {
        if tail_start <= final_line_start {
            tail = input[tail_start..]
                .find('\n')
                .map_or(tail_start, |at| tail_start + at + 1);
        } else if input.len() - final_line_start <= budget {
            tail = final_line_start;
            let head_budget = budget - (input.len() - final_line_start);
            if head > head_budget {
                head = line_start_at_or_before(floor_char_boundary(input, head_budget));
            }
        }
    }
    (head.min(tail), tail)
}

fn append_marker_line(input: &str, marker: &str) -> PiecedText {
    if input.is_empty() {
        return PiecedText::inserted(marker);
    }
    let mut output = PiecedText::default();
    output.copy(input, 0..input.trim_end().len());
    output.insert("\n");
    output.insert(marker);
    output
}

fn marker_capped_output(
    input: &str,
    head_end: usize,
    tail_start: usize,
    marker: &str,
    threshold_bytes: usize,
) -> PiecedText {
    let output = marker_output(input, head_end, tail_start, marker);
    if output.text.len() <= threshold_bytes {
        return output;
    }

    let separator_len = usize::from(head_end > 0 && !input[..head_end].ends_with('\n'));
    let marker_line_len = marker.len().saturating_add(1);
    let tail_budget = threshold_bytes.saturating_sub(head_end + separator_len + marker_line_len);
    let adjusted_tail_start = ceil_char_boundary(input, input.len().saturating_sub(tail_budget));
    let output = marker_output(input, head_end, tail_start.max(adjusted_tail_start), marker);
    if output.text.len() <= threshold_bytes {
        return output;
    }

    // If the marker itself still fits, prefer preserving it and trimming all head
    // content before resorting to a marker-only hard cap.
    let tail_budget = threshold_bytes.saturating_sub(marker_line_len);
    let adjusted_tail_start = ceil_char_boundary(input, input.len().saturating_sub(tail_budget));
    let output = marker_output(input, 0, adjusted_tail_start, marker);
    if output.text.len() <= threshold_bytes {
        return output;
    }

    PiecedText::inserted(&marker[..floor_char_boundary(marker, threshold_bytes.min(marker.len()))])
}

fn marker_output(input: &str, head_end: usize, tail_start: usize, marker: &str) -> PiecedText {
    let mut output = PiecedText::default();
    output.copy(input, 0..head_end);
    if head_end > 0 && !output.text.ends_with('\n') {
        output.insert("\n");
    }
    output.insert(marker);
    if tail_start < input.len() {
        output.insert("\n");
        output.copy(input, tail_start..input.len());
    }
    output
}

pub fn quote_path(path: &str) -> String {
    // The path is shown to the agent as `read "<path>"` for the AFT read tool
    // (and, for the Unix-only `tail -n +N` form, a shell hint). The read tool
    // consumes the path literally, so we must NOT backslash-double: on Windows
    // `C:\Users\foo` would become `C:\\Users\\foo` and the agent would copy a
    // corrupted path. Only escape an embedded `"` so the surrounding quotes
    // can't be broken (paths containing `"` are illegal on Windows and rare on
    // Unix). Realistic Unix paths have no backslashes, so this is a no-op there.
    let escaped = path.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

pub fn json_output_pointer(total_bytes: u64, path: &str) -> String {
    let kb = total_bytes.div_ceil(1024);
    format!(
        "[JSON output {kb} KB; full output: read {}]",
        quote_path(path)
    )
}

pub fn retained_json_output_pointer(
    total_bytes: u64,
    path: &str,
    truncated_prefix_bytes: u64,
) -> String {
    let kb = total_bytes.div_ceil(1024);
    format!(
        "[JSON output {kb} KB; truncated {truncated_prefix_bytes} bytes from saved output prefix; retained output: read {}]",
        quote_path(path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_pieces_rebuild_the_capped_text_from_input_ranges() {
        let cargo = format!(
            "{}running 3 tests\ntest a ... ok\ntest b ... FAILED\n\nfailures:\n    b\n\ntest result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n\n     Running unittests src/empty.rs\n\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n\nerror: test failed\n",
            "   Compiling crate v0.1.0\n".repeat(200)
        );
        let inputs = [
            "short\n".to_string(),
            format!("{}\n", "x".repeat(10_000)),
            format!("{}tail line\n", "line of output\n".repeat(500)),
            format!("{}final\r\n", "crlf line\r\n".repeat(400)),
            format!("{}\n\n\n{}", "a".repeat(700), "b".repeat(3000)),
            cargo,
        ];
        for input in &inputs {
            let last = input.trim_end().lines().last().unwrap().to_string();
            for exit_ok in [true, false] {
                for marker in [None, Some("[full output: read /tmp/stdout]")] {
                    for final_lines in [Vec::new(), vec![last.clone()]] {
                        let (pieced, truncated) = cap_completion_output_preserving_pieces(
                            input,
                            marker,
                            exit_ok,
                            &final_lines,
                        );
                        let rebuilt: String = pieced
                            .pieces
                            .iter()
                            .map(|piece| match piece {
                                CapPiece::Input(range) => &input[range.clone()],
                                CapPiece::Inserted(text) => text.as_str(),
                            })
                            .collect();
                        assert_eq!(rebuilt, pieced.text);
                        assert!(truncated || pieced.text == *input);
                    }
                }
            }
        }
    }

    /// A failed command keeps at most twenty failure lines from the cut
    /// middle, the first ones in output order, inside the reply cap. The scan
    /// stops at the twentieth: lines after it are never examined.
    #[test]
    fn failed_reply_cap_keeps_the_first_twenty_middle_failure_lines() {
        let mut input = "head line\n".repeat(1_000);
        for index in 0..30 {
            input.push_str(&format!("error: middle failure {index}\n"));
        }
        input.push_str(&"middle line\n".repeat(50_000));
        input.push_str(&"tail line\n".repeat(100));
        input.push_str("final verdict\n");
        let final_lines = vec!["final verdict".to_string()];
        let marker = "[cut]";

        let succeeded = cap_final_output_preserving(&input, Some(marker), &final_lines, false);
        assert!(!succeeded.text.contains("middle failure"));
        assert!(!succeeded
            .text
            .contains(crate::compress::PRESERVED_FAILURES_LABEL));

        RESCUE_LINES_EXAMINED.with(|count| count.set(0));
        let failed = cap_final_output_preserving(&input, Some(marker), &final_lines, true);
        let examined = RESCUE_LINES_EXAMINED.with(std::cell::Cell::get);
        assert!(
            failed.text.len() <= FINAL_OUTPUT_CAP_BYTES,
            "{}",
            failed.text.len()
        );
        let (_, rescued) = failed
            .text
            .split_once(&format!("{}\n", crate::compress::PRESERVED_FAILURES_LABEL))
            .expect("label");
        let expected: Vec<String> = (0..20)
            .map(|index| format!("error: middle failure {index}"))
            .chain(["final verdict".to_string()])
            .collect();
        assert_eq!(rescued.lines().collect::<Vec<_>>(), expected);
        // The head keeps most of the 1,000 head lines, so the scan starts in
        // the head's tail and ends at the twentieth failure line; it never
        // reaches the 50,000 middle lines. Three passes at most.
        assert!(examined < 3 * 1_100, "{examined} lines examined");
    }

    #[test]
    fn retained_json_pointer_does_not_claim_full_output() {
        let pointer = retained_json_output_pointer(2048, "/tmp/stdout", 4096);

        assert!(pointer.contains("truncated 4096 bytes from saved output prefix"));
        assert!(pointer.contains(r#"retained output: read "/tmp/stdout""#));
        assert!(!pointer.contains("full output"));
    }

    #[test]
    fn quote_path_preserves_windows_backslashes() {
        // The recovery marker shows the agent `read "<path>"` for the AFT read
        // tool, which consumes the path literally. A Windows path must survive
        // verbatim (no backslash doubling), otherwise the agent copies a broken
        // path. Regression for the cross-platform CI failure.
        assert_eq!(
            quote_path(r"C:\Users\foo\stdout"),
            r#""C:\Users\foo\stdout""#
        );
        assert_eq!(quote_path(r"C:\a\b"), r#""C:\a\b""#);
        assert_eq!(quote_path("/tmp/out"), r#""/tmp/out""#);
        // An embedded double-quote is still escaped so the wrapping can't break.
        assert_eq!(quote_path(r#"a"b"#), r#""a\"b""#);
    }

    #[test]
    fn head_tail_cap_respects_utf8_boundaries() {
        let input = format!("{}{}", "🦀".repeat(4_000), "tail");
        let capped = cap_head_tail(&input, 128, 64, 64);
        assert!(capped.truncated);
        assert!(capped.text.is_char_boundary(capped.text.len()));
        assert!(capped.text.contains("...<truncated "));
        assert!(capped.text.len() <= 128);
        assert!(capped.text.ends_with("tail"));
    }

    #[test]
    fn marker_cap_respects_newline_boundary_oscillation() {
        let capped = cap_head_tail_with_marker("01234567\nabcdef", 20, 15, 0, "123456789");

        assert!(capped.truncated);
        assert!(capped.text.len() <= 20, "{}", capped.text.len());
        assert!(capped.text.is_char_boundary(capped.text.len()));
        assert!(!capped.text.starts_with('\n'));
    }

    #[test]
    fn marker_cap_respects_tiny_threshold_and_long_marker() {
        let capped = cap_head_tail_with_marker("abcdef", 5, 0, 0, "[very long recovery marker]");

        assert!(capped.truncated);
        assert!(capped.text.len() <= 5, "{}", capped.text.len());
        assert!(capped.text.is_char_boundary(capped.text.len()));
        assert!(!capped.text.starts_with('\n'));
    }

    #[test]
    fn marker_cap_accounts_for_marker_when_body_is_under_threshold() {
        let capped = cap_head_tail_with_marker("0123456789", 10, 10, 0, "[x]");

        assert!(capped.truncated);
        assert!(capped.text.len() <= 10, "{}", capped.text.len());
        assert!(capped.text.contains("[x]"));
    }

    #[test]
    fn marker_cap_cuts_on_line_boundaries() {
        let input: String = (1..=2_000).map(|index| format!("{index}\n")).collect();
        let capped = cap_head_tail_with_marker(&input, 1_000, 300, 600, "[cut]");
        assert!(capped.text.len() <= 1_000, "{}", capped.text.len());
        let (head, tail) = capped.text.split_once("[cut]\n").expect("marker line");
        assert!(head.ends_with('\n'), "head ends on a whole line: {head:?}");
        let numbers = |text: &str| -> Vec<usize> {
            text.lines()
                .map(|line| line.parse().expect("whole number line"))
                .collect()
        };
        let head = numbers(head);
        let tail = numbers(tail);
        assert_eq!(head[0], 1);
        assert!(head.windows(2).all(|pair| pair[1] == pair[0] + 1));
        assert!(tail.windows(2).all(|pair| pair[1] == pair[0] + 1));
        assert_eq!(tail.last(), Some(&2_000));
    }

    #[test]
    fn marker_cap_keeps_an_oversized_final_line_whole_when_it_fits() {
        let verdict = format!("VERDICT {}", "v".repeat(700));
        let input = format!("{}{verdict}\n", "noise line\n".repeat(200));
        let capped = cap_head_tail_with_marker(&input, 1_000, 300, 600, "[cut]");
        assert!(capped.text.len() <= 1_000, "{}", capped.text.len());
        assert!(
            capped.text.ends_with(&format!("[cut]\n{verdict}\n")),
            "{}",
            capped.text
        );
        assert!(capped.text.starts_with("noise line\n"));
    }

    #[test]
    fn completion_cap_success_is_tail_only_and_small() {
        // A successful task's reminder preview keeps a short tail — no head.
        // Regression: the uniform 4 KiB head+tail cap flooded completion
        // reminders with build noise (~1K tokens per task).
        let input = format!("HEAD-NOISE\n{}\nFINAL SUMMARY LINE", "x".repeat(8_000));
        let capped = cap_completion_output(&input, true);
        assert!(capped.truncated);
        assert!(
            capped.text.len() <= COMPLETION_SUCCESS_PREVIEW_BYTES,
            "{}",
            capped.text.len()
        );
        assert!(capped.text.ends_with("FINAL SUMMARY LINE"));
        assert!(!capped.text.contains("HEAD-NOISE"));
    }

    #[test]
    fn completion_cap_failure_keeps_head_and_larger_tail() {
        // A failed task keeps a small head (command banner / first error) and
        // a meaningful tail (tracebacks land at the end).
        let input = format!(
            "FIRST-ERROR-CONTEXT\n{}\nTraceback: ModuleNotFoundError",
            "y".repeat(8_000)
        );
        let capped = cap_completion_output(&input, false);
        assert!(capped.truncated);
        assert!(
            capped.text.len() <= COMPLETION_FAILURE_PREVIEW_BYTES,
            "{}",
            capped.text.len()
        );
        assert!(capped.text.starts_with("FIRST-ERROR-CONTEXT"));
        assert!(capped.text.ends_with("Traceback: ModuleNotFoundError"));
        // Failure budget is larger than success budget but far below the old 4 KiB.
        const {
            assert!(COMPLETION_FAILURE_PREVIEW_BYTES > COMPLETION_SUCCESS_PREVIEW_BYTES);
            assert!(COMPLETION_FAILURE_PREVIEW_BYTES < 4 * 1024);
        }
    }

    #[test]
    fn completion_cap_passes_short_output_through_untouched() {
        let input = "ok: 12 tests passed";
        for exit_ok in [true, false] {
            let capped = cap_completion_output(input, exit_ok);
            assert!(!capped.truncated);
            assert_eq!(capped.text, input);
        }
    }

    #[test]
    fn cargo_verdict_caps_do_not_change_other_command_output() {
        let input = format!("{}last line\n", "ordinary output\n".repeat(4000));
        let marker = "[truncated output; full output unavailable]";
        assert_eq!(
            cap_final_output(&input),
            cap_head_tail(
                &input,
                FINAL_OUTPUT_CAP_BYTES,
                FINAL_OUTPUT_HEAD_BYTES,
                FINAL_OUTPUT_TAIL_BYTES
            )
        );
        assert_eq!(
            cap_final_output_with_marker(&input, marker),
            cap_head_tail_with_marker(
                &input,
                FINAL_OUTPUT_CAP_BYTES,
                FINAL_OUTPUT_HEAD_BYTES,
                FINAL_OUTPUT_TAIL_BYTES,
                marker
            )
        );
        for exit_ok in [true, false] {
            let (threshold, head, tail) = completion_caps(exit_ok);
            assert_eq!(
                cap_completion_output(&input, exit_ok),
                cap_head_tail(&input, threshold, head, tail)
            );
            assert_eq!(
                cap_completion_output_with_marker(&input, marker, exit_ok),
                cap_head_tail_with_marker(&input, threshold, head, tail, marker)
            );
        }
    }

    #[test]
    fn cargo_verdict_names_outgrow_soft_cap_but_panic_locations_are_bounded() {
        let mut input = String::new();
        for index in 0..300 {
            input.push_str(&format!("test long_module_name::case_{index} ... FAILED\nthread 'case_{index}' panicked at src/lib.rs:{index}:1:\n{}", "backtrace\n".repeat(20)));
        }
        input.push_str("test result: FAILED. 0 passed; 300 failed\nwrapper verdict\n");
        let capped = cap_completion_output(&input, false);
        for index in 0..300 {
            assert!(capped
                .text
                .contains(&format!("test long_module_name::case_{index} ... FAILED")));
        }
        assert!(capped
            .text
            .contains("test result: FAILED. 0 passed; 300 failed"));
        assert!(capped.text.ends_with("wrapper verdict"));
        assert_eq!(capped.text.matches("panicked at").count(), 20);
        assert!(capped.text.len() > COMPLETION_FAILURE_PREVIEW_BYTES);
    }
}
