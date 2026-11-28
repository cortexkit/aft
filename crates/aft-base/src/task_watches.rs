use regex::{Regex, RegexBuilder};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::task_watch_lines::{LineBatch, LineDelivery};

const MAX_WATCHES_PER_TASK: usize = 8;
const CONTEXT_BEFORE: usize = 100;
const CONTEXT_AFTER: usize = 500;
/// Bytes before a stream's cursor that each scan searches again, so a match
/// split across two reads is still found.
pub const SCAN_OVERLAP_BYTES: usize = 8 * 1024;

pub const WATCH_TARGET_ERASED_TEXT: &str = "watch target erased";
pub const WATCH_TARGET_ERASED_CONTEXT: &str =
    "watch target erased: the background task row was erased before the watch reached a normal terminal result";
pub const WATCH_TASK_EXIT_TEXT: &str = "watch task exited";
/// The line a sticky watch's notification adds when more matches of the watch
/// followed in the same scanned output. `{n}` is their count.
pub const MORE_MATCHES_TEXT: &str =
    "{n} more matches followed in the same output; read them with bash_status.";

/// [`MORE_MATCHES_TEXT`] for `more` further matches, or `None` for none.
pub fn more_matches_line(more: u64) -> Option<String> {
    (more > 0).then(|| MORE_MATCHES_TEXT.replace("{n}", &more.to_string()))
}

#[derive(Debug, Clone)]
pub struct WatchSpec {
    pub watch_id: String,
    pub task_id: String,
    pub pattern: WatchPattern,
    pub once: bool,
}

#[derive(Debug, Clone)]
pub enum WatchPattern {
    Substring(String),
    Regex(Regex),
}

impl WatchPattern {
    pub fn regex(pattern: &str) -> Result<Self, regex::Error> {
        RegexBuilder::new(pattern)
            .multi_line(true)
            .build()
            .map(Self::Regex)
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Substring(_) => "substring",
            Self::Regex(_) => "regex",
        }
    }

    pub fn pattern_text(&self) -> &str {
        match self {
            Self::Substring(text) => text,
            Self::Regex(regex) => regex.as_str(),
        }
    }

    pub fn from_persisted(kind: &str, pattern: &str) -> Result<Self, String> {
        match kind {
            "substring" => Ok(Self::Substring(pattern.to_string())),
            "regex" => Self::regex(pattern).map_err(|error| error.to_string()),
            other => Err(format!("unknown pattern kind {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PatternMatch {
    #[serde(skip)]
    pub durable_pending: bool,
    pub watch_id: String,
    pub task_id: String,
    pub match_text: String,
    pub match_offset: u64,
    pub context: String,
    pub once: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<LineBatch>,
    /// The watch is a substring watch, so `match_text` is the pattern exactly
    /// as the agent supplied it rather than text taken from the output.
    /// Presentation masking must leave it as it is.
    #[serde(skip)]
    pub literal_pattern: bool,
}

impl PatternMatch {
    pub fn copy_delivery_identity(&mut self, persisted: &Self) {
        self.durable_pending = persisted.durable_pending;
        if let (Some(live), Some(saved)) = (self.batch.as_mut(), persisted.batch.as_ref()) {
            self.match_offset = persisted.match_offset;
            live.pending_id = saved.pending_id.clone();
            live.version = saved.version;
        }
    }
}

#[derive(Debug, Default)]
pub struct WatchRegistry {
    line_delivery: HashMap<String, LineDelivery>,
    watches: HashMap<String, Vec<WatchSpec>>,
    scan_cursors: HashMap<String, u64>,
    scan_overlaps: HashMap<String, Vec<u8>>,
    /// Cursor keys whose last scan held back a regex match that reached the
    /// end of the bytes read so far (see [`find_match`]).
    held_ends: HashSet<String>,
    controlled_tasks: HashSet<String>,
    matched_tasks: HashSet<String>,
    /// Process-local tombstones keep `bash_status` informative after the durable
    /// task and watch rows are gone. They intentionally disappear on restart.
    erased_notifications: HashSet<String>,
    next_watch: u64,
}

impl WatchRegistry {
    pub fn enable_each_line(&mut self, watch_id: &str) {
        self.line_delivery.entry(watch_id.to_string()).or_default();
    }

    pub fn is_each_line(&self, watch_id: &str) -> bool {
        self.line_delivery.contains_key(watch_id)
    }

    pub fn replay_cursor(&self, task_id: &str, stream: &str, cursor: u64) -> u64 {
        self.watch_ids(task_id)
            .iter()
            .filter_map(|id| self.line_delivery.get(id))
            .fold(cursor, |cursor, delivery| {
                delivery.replay_cursor(stream, cursor)
            })
    }

    pub fn take_line_batches(&mut self, task_id: &str, terminal: bool) -> Vec<PatternMatch> {
        self.take_line_batches_at(task_id, terminal, Instant::now())
    }

    fn take_line_batches_at(
        &mut self,
        task_id: &str,
        terminal: bool,
        now: Instant,
    ) -> Vec<PatternMatch> {
        let mut found = Vec::new();
        for watch in self.watch_specs(task_id) {
            let Some(delivery) = self.line_delivery.get_mut(&watch.watch_id) else {
                continue;
            };
            let Some(batch) = delivery.take(now, terminal, &watch.pattern) else {
                continue;
            };
            let matched = batch.has_matches();
            let offset = batch
                .lines
                .first()
                .map(|line| line.offset)
                .or(batch.first_dropped_offset)
                .unwrap_or(0);
            if matched {
                self.matched_tasks.insert(task_id.to_string());
            }
            found.push(PatternMatch {
                durable_pending: false,
                watch_id: watch.watch_id.clone(),
                task_id: task_id.to_string(),
                match_text: "matching lines".to_string(),
                match_offset: offset,
                context: String::new(),
                once: watch.once && matched,
                batch: Some(batch),
                // A batch's rows are rendered from the output separately.
                literal_pattern: false,
            });
            if watch.once && matched {
                self.unregister(task_id, &watch.watch_id);
            }
        }
        found
    }
    pub fn register(
        &mut self,
        task_id: String,
        pattern: WatchPattern,
        once: bool,
    ) -> Result<String, &'static str> {
        let watches = self.watches.entry(task_id.clone()).or_default();
        if watches.len() >= MAX_WATCHES_PER_TASK {
            return Err("too_many_watches");
        }
        self.controlled_tasks.insert(task_id.clone());
        self.next_watch = self.next_watch.wrapping_add(1);
        let watch_id = format!("watch-{:08x}", self.next_watch);
        watches.push(WatchSpec {
            watch_id: watch_id.clone(),
            task_id,
            pattern,
            once,
        });
        Ok(watch_id)
    }

    /// Re-arm a previously persisted watch after bridge/daemon restart.
    ///
    /// `scanning=false` means a once-watch already matched and is only kept so
    /// a pending notification can be re-delivered until ack; it does not scan.
    pub fn restore(
        &mut self,
        watch_id: String,
        task_id: String,
        pattern: WatchPattern,
        once: bool,
        scanning: bool,
    ) -> Result<(), &'static str> {
        self.note_restored_watch_id(&watch_id);
        self.controlled_tasks.insert(task_id.clone());
        if !scanning {
            self.matched_tasks.insert(task_id);
            return Ok(());
        }
        let watches = self.watches.entry(task_id.clone()).or_default();
        if watches.iter().any(|watch| watch.watch_id == watch_id) {
            return Ok(());
        }
        if watches.len() >= MAX_WATCHES_PER_TASK {
            return Err("too_many_watches");
        }
        watches.push(WatchSpec {
            watch_id,
            task_id,
            pattern,
            once,
        });
        Ok(())
    }

    fn note_restored_watch_id(&mut self, watch_id: &str) {
        let Some(hex) = watch_id.strip_prefix("watch-") else {
            return;
        };
        if let Ok(value) = u64::from_str_radix(hex, 16) {
            if value >= self.next_watch {
                self.next_watch = value;
            }
        }
    }

    pub fn unregister(&mut self, task_id: &str, watch_id: &str) {
        self.line_delivery.remove(watch_id);
        if let Some(watches) = self.watches.get_mut(task_id) {
            watches.retain(|watch| watch.watch_id != watch_id);
            if watches.is_empty() {
                self.watches.remove(task_id);
            }
        }
    }

    pub fn file_cursor(&self, cursor_key: &str) -> Option<u64> {
        self.scan_cursors.get(cursor_key).copied()
    }

    pub fn watch_ids(&self, task_id: &str) -> Vec<String> {
        self.watches
            .get(task_id)
            .map(|watches| watches.iter().map(|watch| watch.watch_id.clone()).collect())
            .unwrap_or_default()
    }

    pub fn watched_task_ids(&self) -> Vec<String> {
        self.watches.keys().cloned().collect()
    }

    /// How many tasks have watches registered.
    pub fn watched_task_count(&self) -> usize {
        self.watches.len()
    }

    pub fn watch_specs(&self, task_id: &str) -> Vec<WatchSpec> {
        self.watches.get(task_id).cloned().unwrap_or_default()
    }

    /// Reconcile process-local watch state with the durable rows shared by actors.
    pub fn reconcile_watch_ids(
        &mut self,
        task_id: &str,
        keep: &HashSet<String>,
        has_pending_match: bool,
    ) {
        for watch_id in self.watch_ids(task_id) {
            if !keep.contains(&watch_id) {
                self.line_delivery.remove(&watch_id);
            }
        }
        let watches_empty = if let Some(watches) = self.watches.get_mut(task_id) {
            watches.retain(|watch| keep.contains(&watch.watch_id));
            watches.is_empty()
        } else {
            true
        };
        if watches_empty {
            self.clear_task(task_id);
        } else if has_pending_match {
            self.matched_tasks.insert(task_id.to_string());
        } else {
            self.matched_tasks.remove(task_id);
        }
    }

    pub fn clear_task(&mut self, task_id: &str) {
        for watch_id in self.watch_ids(task_id) {
            self.line_delivery.remove(&watch_id);
        }
        self.watches.remove(task_id);
        self.controlled_tasks.remove(task_id);
        self.matched_tasks.remove(task_id);
        let prefix = format!("{task_id}:");
        self.scan_cursors
            .retain(|key, _| key != task_id && !key.starts_with(&prefix));
        self.scan_overlaps
            .retain(|key, _| key != task_id && !key.starts_with(&prefix));
        self.held_ends
            .retain(|key| key != task_id && !key.starts_with(&prefix));
    }

    pub fn terminalize_erased_task(&mut self, task_id: &str) -> Vec<String> {
        let watch_ids = self.watch_ids(task_id);
        self.clear_task(task_id);
        watch_ids
            .into_iter()
            .filter(|watch_id| {
                self.erased_notifications
                    .insert(format!("{task_id}:{watch_id}"))
            })
            .collect()
    }

    pub fn has_erased_task(&self, task_id: &str) -> bool {
        let prefix = format!("{task_id}:");
        self.erased_notifications
            .iter()
            .any(|notification| notification.starts_with(&prefix))
    }

    pub fn forget_erased_task(&mut self, task_id: &str) {
        let prefix = format!("{task_id}:");
        self.erased_notifications
            .retain(|notification| !notification.starts_with(&prefix));
    }

    pub fn has_controlled_task(&self, task_id: &str) -> bool {
        self.controlled_tasks.contains(task_id)
    }

    pub fn has_matched_task(&self, task_id: &str) -> bool {
        self.matched_tasks.contains(task_id)
    }

    pub fn active_count(&self, task_id: &str) -> usize {
        self.watches.get(task_id).map_or(0, Vec::len)
    }

    pub fn set_file_cursor(&mut self, cursor_key: &str, offset: u64) {
        self.scan_cursors.insert(cursor_key.to_string(), offset);
        self.scan_overlaps.remove(cursor_key);
        self.held_ends.remove(cursor_key);
    }

    /// Resume a stream's scanning at a saved `offset` after a restart, as if
    /// the scans before it had run in this process. `before` holds the
    /// stream's bytes just before `offset`; up to [`SCAN_OVERLAP_BYTES`] of
    /// them become the overlap, so a match spanning the saved cursor is still
    /// found and one that ended before it is not reported again.
    ///
    /// Only the cursor is saved, not whether the last scan held back a regex
    /// match ending at it. While the task `running`, a scan reports no regex
    /// match ending exactly at the end of the bytes read, so any such match
    /// was held and is admitted again here. A finished task's scans reported
    /// those matches already.
    pub fn restore_file_cursor(
        &mut self,
        cursor_key: &str,
        offset: u64,
        before: &[u8],
        running: bool,
    ) {
        self.set_file_cursor(cursor_key, offset);
        let keep = before.len().min(SCAN_OVERLAP_BYTES);
        if keep == 0 {
            return;
        }
        self.scan_overlaps.insert(
            cursor_key.to_string(),
            before[before.len() - keep..].to_vec(),
        );
        if running {
            self.held_ends.insert(cursor_key.to_string());
        }
    }

    /// Scan `bytes`, read from a stream at `start`, for every watch of
    /// `task_id`. `at_end` says the stream ends with them: the task finished
    /// and nothing more will be written. Until then a regex match reaching the
    /// end of the bytes is held back; an `at_end` scan of no bytes reports it.
    /// Each match comes with the number of further matches of the same watch
    /// in these bytes, which its one notification stands for (always 0 for a
    /// `once` watch, which ends at its first match).
    pub fn scan_chunk(
        &mut self,
        cursor_key: &str,
        task_id: &str,
        bytes: &[u8],
        start: u64,
        at_end: bool,
        render: &mut dyn FnMut(&str, usize, usize, u64, u64) -> (String, String),
    ) -> Vec<(PatternMatch, u64)> {
        if bytes.is_empty() && !self.held_ends.contains(cursor_key) {
            return Vec::new();
        }
        self.scan_cursors.insert(
            cursor_key.to_string(),
            start.saturating_add(bytes.len() as u64),
        );
        self.scan_new_bytes_at(cursor_key, task_id, bytes, start, at_end, render)
    }

    pub fn scan_new_bytes(&mut self, task_id: &str, bytes: &[u8]) -> Vec<PatternMatch> {
        let base = self.scan_cursors.get(task_id).copied().unwrap_or(0);
        self.scan_cursors
            .insert(task_id.to_string(), base.saturating_add(bytes.len() as u64));
        self.scan_new_bytes_at(
            task_id,
            task_id,
            bytes,
            base,
            false,
            &mut |text, start, end, _, _| {
                (
                    text[start..end].to_string(),
                    context_snippet(text, start, end),
                )
            },
        )
        .into_iter()
        .map(|(found, _)| found)
        .collect()
    }

    fn scan_new_bytes_at(
        &mut self,
        cursor_key: &str,
        task_id: &str,
        bytes: &[u8],
        base_offset: u64,
        at_end: bool,
        render: &mut dyn FnMut(&str, usize, usize, u64, u64) -> (String, String),
    ) -> Vec<(PatternMatch, u64)> {
        let Some(watches) = self.watches.get(task_id).cloned() else {
            return Vec::new();
        };
        let end_held = self.held_ends.remove(cursor_key);
        let mut held = false;
        let overlap = self
            .scan_overlaps
            .get(cursor_key)
            .cloned()
            .unwrap_or_default();
        let prefix_len = overlap.len();
        let mut scan_bytes = Vec::with_capacity(prefix_len.saturating_add(bytes.len()));
        scan_bytes.extend_from_slice(&overlap);
        scan_bytes.extend_from_slice(bytes);
        let text = String::from_utf8_lossy(&scan_bytes);
        let offsets = LossyOffsets::new(&scan_bytes);
        let scan_base_offset = base_offset.saturating_sub(prefix_len as u64);
        let mut matches = Vec::new();
        let mut remove_once = Vec::new();
        for watch in watches {
            if let Some(delivery) = self.line_delivery.get_mut(&watch.watch_id) {
                let stream = cursor_key.rsplit(':').next().unwrap_or("stdout");
                delivery.scan(stream, bytes, base_offset, &watch.pattern);
                continue;
            }
            if let Some(((text_start, text_end, source_start, source_end), more)) = find_match(
                &watch.pattern,
                &text,
                &offsets,
                prefix_len,
                end_held,
                at_end,
                !watch.once,
                &mut held,
            ) {
                self.matched_tasks.insert(task_id.to_string());
                let (rendered_match, context) = render(
                    &text,
                    text_start,
                    text_end,
                    scan_base_offset.saturating_add(source_start as u64),
                    scan_base_offset.saturating_add(source_end as u64),
                );
                // A substring watch matches exactly the text the agent gave it,
                // which the agent already has, so it is shown as given. Only a
                // regex match is text taken from the output and may be masked.
                let (match_text, literal_pattern) = match &watch.pattern {
                    WatchPattern::Substring(needle) => (needle.clone(), true),
                    WatchPattern::Regex(_) => (rendered_match, false),
                };
                matches.push((
                    PatternMatch {
                        durable_pending: false,
                        watch_id: watch.watch_id.clone(),
                        task_id: watch.task_id.clone(),
                        match_text,
                        match_offset: scan_base_offset.saturating_add(source_start as u64),
                        context,
                        once: watch.once,
                        batch: None,
                        literal_pattern,
                    },
                    more,
                ));
                if watch.once {
                    remove_once.push(watch.watch_id);
                }
            }
        }
        for watch_id in remove_once {
            self.unregister(task_id, &watch_id);
        }
        if held {
            self.held_ends.insert(cursor_key.to_string());
        }
        let keep = scan_bytes.len().min(SCAN_OVERLAP_BYTES);
        self.scan_overlaps.insert(
            cursor_key.to_string(),
            scan_bytes[scan_bytes.len().saturating_sub(keep)..].to_vec(),
        );
        matches
    }
}

/// The first match of `pattern` in `text` (`source` decoded) that ends past
/// the `prefix_len` overlap bytes an earlier scan already searched, and, when
/// `count_more`, how many further matches follow it in `text`.
///
/// A regex match that reaches the end of `text` before the stream ends may
/// depend on what follows (`$`, `\b`, a greedy repeat such as `\d+`), so it is
/// held back: `held` is set and the match is skipped. The overlap keeps its
/// bytes, and the next scan finds it again ending exactly at `prefix_len`,
/// which `end_held` then admits. A literal is complete once its bytes are in.
#[allow(clippy::too_many_arguments)]
fn find_match(
    pattern: &WatchPattern,
    text: &str,
    offsets: &LossyOffsets,
    prefix_len: usize,
    end_held: bool,
    at_end: bool,
    count_more: bool,
    held: &mut bool,
) -> Option<((usize, usize, usize, usize), u64)> {
    let regex = matches!(pattern, WatchPattern::Regex(_));
    let min_source_end_exclusive = if regex && end_held {
        prefix_len.saturating_sub(1)
    } else {
        prefix_len
    };
    let still_written = |end: usize| regex && !at_end && end == text.len();
    let mut candidates: Box<dyn Iterator<Item = (usize, usize)>> = match pattern {
        WatchPattern::Substring(needle) if needle.is_empty() => return None,
        WatchPattern::Substring(needle) => Box::new(
            text.match_indices(needle.as_str())
                .map(|(start, matched)| (start, start + matched.len())),
        ),
        WatchPattern::Regex(regex) => Box::new(
            regex
                .find_iter(text)
                .map(|found| (found.start(), found.end())),
        ),
    };
    let first = candidates.by_ref().find_map(|(start, end)| {
        if still_written(end) {
            *held = true;
            return None;
        }
        let source_end = offsets.source_offset(end);
        (source_end > min_source_end_exclusive)
            .then(|| (start, end, offsets.source_offset(start), source_end))
    })?;
    // Matches do not overlap, so every later one ends after the first and so
    // past the overlap. A later regex match still being written is held for
    // the next scan instead of counted.
    let more = if count_more {
        candidates
            .filter(|&(_, end)| {
                let written = !still_written(end);
                *held |= !written;
                written
            })
            .count() as u64
    } else {
        0
    };
    Some((first, more))
}

/// Maps byte offsets in `String::from_utf8_lossy(source)` back to `source`.
/// Built once per scan, so each match costs a binary search rather than a
/// fresh decode of the whole scan buffer.
struct LossyOffsets {
    /// Runs of `source` that decode byte for byte, each but the last followed
    /// by an invalid sequence that renders as one U+FFFD.
    runs: Vec<LossyRun>,
    source_len: usize,
}

struct LossyRun {
    rendered: usize,
    source: usize,
    valid: usize,
    /// Bytes of the invalid sequence after the run; 0 for the final run.
    invalid: usize,
}

impl LossyOffsets {
    fn new(source: &[u8]) -> Self {
        let replacement = '\u{FFFD}'.len_utf8();
        let mut runs = Vec::new();
        let (mut source_start, mut rendered_start) = (0, 0);
        while source_start < source.len() {
            #[cfg(test)]
            count_validated_bytes(source.len() - source_start);
            match std::str::from_utf8(&source[source_start..]) {
                Ok(valid) => {
                    runs.push(LossyRun {
                        rendered: rendered_start,
                        source: source_start,
                        valid: valid.len(),
                        invalid: 0,
                    });
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    let invalid = error
                        .error_len()
                        .unwrap_or(source.len() - source_start - valid);
                    runs.push(LossyRun {
                        rendered: rendered_start,
                        source: source_start,
                        valid,
                        invalid,
                    });
                    source_start += valid + invalid;
                    rendered_start += valid + replacement;
                }
            }
        }
        Self {
            runs,
            source_len: source.len(),
        }
    }

    /// The `source` offset of byte `lossy_offset` of the decoded text. An
    /// offset inside a U+FFFD maps to the start of the invalid sequence it
    /// stands for.
    fn source_offset(&self, lossy_offset: usize) -> usize {
        let Some(run) = self
            .runs
            .partition_point(|run| run.rendered <= lossy_offset)
            .checked_sub(1)
            .map(|index| &self.runs[index])
        else {
            return self.source_len;
        };
        let offset = lossy_offset - run.rendered;
        if run.invalid == 0 {
            run.source + offset.min(run.valid)
        } else if offset <= run.valid {
            run.source + offset
        } else if offset < run.valid + '\u{FFFD}'.len_utf8() {
            run.source + run.valid
        } else {
            run.source + run.valid + run.invalid
        }
    }
}

#[cfg(test)]
thread_local! {
    static VALIDATED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Bytes handed to UTF-8 validation while mapping match offsets.
#[cfg(test)]
fn count_validated_bytes(bytes: usize) {
    VALIDATED_BYTES.with(|count| count.set(count.get() + bytes));
}

pub fn context_snippet(text: &str, start: usize, end: usize) -> String {
    text[context_range(text, start, end)].replace('\r', "")
}

pub fn context_range(text: &str, start: usize, end: usize) -> std::ops::Range<usize> {
    let before_start = text[..start]
        .char_indices()
        .rev()
        .nth(CONTEXT_BEFORE)
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    let after_end = text[end..]
        .char_indices()
        .nth(CONTEXT_AFTER)
        .map(|(idx, _)| end + idx)
        .unwrap_or(text.len());
    before_start..after_end
}

#[cfg(test)]
mod tests {
    #[test]
    fn each_line_default_frame_is_unchanged() {
        let mut registry = super::WatchRegistry::default();
        registry
            .register(
                "task".into(),
                super::WatchPattern::Substring("ERROR".into()),
                false,
            )
            .unwrap();
        let found = registry.scan_new_bytes("task", b"ERROR a\nERROR b\n");
        assert_eq!(
            serde_json::to_string(&found).unwrap(),
            r#"[{"watch_id":"watch-00000001","task_id":"task","match_text":"ERROR","match_offset":0,"context":"ERROR a\nERROR b\n","once":false}]"#
        );
    }

    #[test]
    fn each_line_registry_batches_across_pages() {
        let mut registry = super::WatchRegistry::default();
        let id = registry
            .register(
                "task".into(),
                super::WatchPattern::Substring("ERROR".into()),
                false,
            )
            .unwrap();
        registry.enable_each_line(&id);
        registry.scan_new_bytes("task", b"ERROR a\nER");
        registry.scan_new_bytes("task", b"ROR b\n");
        let found = registry.take_line_batches("task", false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].batch.as_ref().unwrap().lines.len(), 2);
    }
    use super::*;

    #[test]
    fn once_watch_self_removes_after_match() {
        let mut registry = WatchRegistry::default();
        let task_id = "bash-1".to_string();
        registry
            .register(
                task_id.clone(),
                WatchPattern::Substring("READY".into()),
                true,
            )
            .unwrap();
        assert_eq!(registry.scan_new_bytes(&task_id, b"READY\n").len(), 1);
        assert_eq!(registry.active_count(&task_id), 0);
    }

    #[test]
    fn sticky_watch_fires_multiple_times() {
        let mut registry = WatchRegistry::default();
        let task_id = "bash-1".to_string();
        registry
            .register(
                task_id.clone(),
                WatchPattern::Substring("READY".into()),
                false,
            )
            .unwrap();
        assert_eq!(registry.scan_new_bytes(&task_id, b"READY\n").len(), 1);
        assert_eq!(registry.scan_new_bytes(&task_id, b"READY\n").len(), 1);
        assert_eq!(registry.active_count(&task_id), 1);
    }

    #[test]
    fn cap_8_watches_per_task_rejects_9th() {
        let mut registry = WatchRegistry::default();
        for _ in 0..8 {
            registry
                .register("bash-1".into(), WatchPattern::Substring("x".into()), true)
                .unwrap();
        }
        assert_eq!(
            registry.register("bash-1".into(), WatchPattern::Substring("x".into()), true),
            Err("too_many_watches")
        );
    }

    #[test]
    fn regex_pattern_matches_with_capture() {
        let mut registry = WatchRegistry::default();
        let task_id = "bash-1".to_string();
        registry
            .register(
                task_id.clone(),
                WatchPattern::regex("port (\\d+)").unwrap(),
                true,
            )
            .unwrap();
        let hits = registry.scan_new_bytes(&task_id, b"listening on port 3000\n");
        assert_eq!(hits[0].match_text, "port 3000");
    }

    #[test]
    fn substring_pattern_can_span_scans() {
        let mut registry = WatchRegistry::default();
        let task_id = "bash-1".to_string();
        registry
            .register(
                task_id.clone(),
                WatchPattern::Substring("READY".into()),
                true,
            )
            .unwrap();

        assert!(registry.scan_new_bytes(&task_id, b"RE").is_empty());
        let hits = registry.scan_new_bytes(&task_id, b"ADY\n");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].match_text, "READY");
        assert_eq!(hits[0].match_offset, 0);
    }

    #[test]
    fn regex_pattern_can_span_scans() {
        let mut registry = WatchRegistry::default();
        let task_id = "bash-1".to_string();
        registry
            .register(
                task_id.clone(),
                WatchPattern::regex("ready: \\d{4}").unwrap(),
                true,
            )
            .unwrap();

        assert!(registry
            .scan_new_bytes(&task_id, b"prefix ready: 4")
            .is_empty());
        let hits = registry.scan_new_bytes(&task_id, b"242\n");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].match_text, "ready: 4242");
        assert_eq!(hits[0].match_offset, 7);
    }

    #[test]
    fn overlap_does_not_repeat_fully_previous_match() {
        let mut registry = WatchRegistry::default();
        let task_id = "bash-1".to_string();
        registry
            .register(
                task_id.clone(),
                WatchPattern::Substring("READY".into()),
                false,
            )
            .unwrap();

        assert_eq!(registry.scan_new_bytes(&task_id, b"READY").len(), 1);
        assert!(registry.scan_new_bytes(&task_id, b"\n").is_empty());
    }

    #[test]
    fn lossy_utf8_indices_are_mapped_back_to_source_offsets() {
        let mut registry = WatchRegistry::default();
        let task_id = "bash-1".to_string();
        registry
            .register(
                task_id.clone(),
                WatchPattern::Substring("READY".into()),
                false,
            )
            .unwrap();

        let hits = registry.scan_new_bytes(&task_id, b"\xffREADY");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].match_offset, 1);
        assert!(registry.scan_new_bytes(&task_id, b"\n").is_empty());
    }

    fn regex_watch(pattern: &str, once: bool) -> WatchRegistry {
        let mut registry = WatchRegistry::default();
        registry
            .register("bash-1".into(), WatchPattern::regex(pattern).unwrap(), once)
            .unwrap();
        registry
    }

    fn texts(hits: &[PatternMatch]) -> Vec<(String, u64)> {
        hits.iter()
            .map(|hit| (hit.match_text.clone(), hit.match_offset))
            .collect()
    }

    #[test]
    fn regex_match_reaching_the_read_end_waits_for_the_rest_of_its_value() {
        let mut registry = regex_watch(r"port (\d+)", true);
        assert!(registry
            .scan_new_bytes("bash-1", b"listening on port 30")
            .is_empty());
        let hits = registry.scan_new_bytes("bash-1", b"00\n");
        assert_eq!(texts(&hits), vec![("port 3000".to_string(), 13)]);
    }

    #[test]
    fn regex_end_anchor_does_not_match_a_line_still_being_written() {
        let mut registry = regex_watch(r"^READY$", true);
        assert!(registry.scan_new_bytes("bash-1", b"boot\nREADY").is_empty());
        assert!(registry
            .scan_new_bytes("bash-1", b"ING to start\n")
            .is_empty());
        assert_eq!(registry.active_count("bash-1"), 1);
        assert!(registry.scan_new_bytes("bash-1", b"READY").is_empty());
        let hits = registry.scan_new_bytes("bash-1", b"\n");
        assert_eq!(texts(&hits), vec![("READY".to_string(), 23)]);
    }

    #[test]
    fn sticky_regex_reports_a_growing_token_once() {
        let mut registry = regex_watch(r"id=\d+", false);
        assert!(registry.scan_new_bytes("bash-1", b"job id=12").is_empty());
        let hits = registry.scan_new_bytes("bash-1", b"34 queued\n");
        assert_eq!(texts(&hits), vec![("id=1234".to_string(), 4)]);
        assert!(registry.scan_new_bytes("bash-1", b"done\n").is_empty());
    }

    #[test]
    fn held_regex_match_is_reported_once_when_the_stream_ends() {
        let mut registry = regex_watch(r"READY$", false);
        let mut render = |text: &str, start: usize, end: usize, _: u64, _: u64| {
            (text[start..end].to_string(), String::new())
        };
        let key = "bash-1:stdout";
        assert!(registry
            .scan_chunk(key, "bash-1", b"boot\nREADY", 0, false, &mut render)
            .is_empty());
        let hits = found(registry.scan_chunk(key, "bash-1", b"", 10, true, &mut render));
        assert_eq!(texts(&hits), vec![("READY".to_string(), 5)]);
        assert!(registry
            .scan_chunk(key, "bash-1", b"", 10, true, &mut render)
            .is_empty());
        // A match ending where a finished stream ends is reported at once.
        let mut registry = regex_watch(r"READY$", false);
        let hits = found(registry.scan_chunk(key, "bash-1", b"boot\nREADY", 0, true, &mut render));
        assert_eq!(texts(&hits), vec![("READY".to_string(), 5)]);
    }

    fn found(hits: Vec<(PatternMatch, u64)>) -> Vec<PatternMatch> {
        hits.into_iter().map(|(hit, _)| hit).collect()
    }

    /// The per-offset decode the scan used before [`LossyOffsets`], kept as
    /// the reference its answers must equal.
    fn decoded_source_offset(source: &[u8], lossy_offset: usize) -> usize {
        let (mut source_start, mut rendered_start) = (0, 0);
        while source_start < source.len() {
            match std::str::from_utf8(&source[source_start..]) {
                Ok(valid) => {
                    return source_start
                        + lossy_offset.saturating_sub(rendered_start).min(valid.len());
                }
                Err(error) => {
                    let valid_len = error.valid_up_to();
                    if lossy_offset <= rendered_start + valid_len {
                        return source_start + lossy_offset.saturating_sub(rendered_start);
                    }
                    source_start += valid_len;
                    rendered_start += valid_len;
                    let invalid_len = error.error_len().unwrap_or(source.len() - source_start);
                    if lossy_offset < rendered_start + 3 {
                        return source_start;
                    }
                    source_start += invalid_len;
                    rendered_start += 3;
                }
            }
        }
        source.len()
    }

    #[test]
    fn lossy_offsets_match_a_fresh_decode_at_every_offset() {
        let mut seed = 0x5eed_u64;
        for round in 0..300 {
            let mut source = Vec::new();
            for _ in 0..(round % 40) {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                // Mostly ASCII, with multi-byte starts and stray continuations.
                source.push(match (seed >> 33) % 8 {
                    0 => 0xe2,
                    1 => 0x82,
                    2 => 0xff,
                    3 => 0xc3,
                    _ => b'a' + (seed >> 40) as u8 % 26,
                });
            }
            let offsets = LossyOffsets::new(&source);
            let rendered = String::from_utf8_lossy(&source).len();
            for lossy in 0..=rendered + 4 {
                assert_eq!(
                    offsets.source_offset(lossy),
                    decoded_source_offset(&source, lossy),
                    "{source:?} at {lossy}"
                );
            }
        }
    }

    #[test]
    fn dense_matches_in_the_overlap_cost_one_decode_per_scan() {
        let mut registry = WatchRegistry::default();
        registry
            .register("bash-1".into(), WatchPattern::Substring("a".into()), false)
            .unwrap();
        let chunk = vec![b'a'; 64 * 1024];
        registry.scan_new_bytes("bash-1", &chunk);
        VALIDATED_BYTES.with(|count| count.set(0));
        // Every byte of the 8 KiB overlap is a rejected candidate.
        assert_eq!(registry.scan_new_bytes("bash-1", &chunk).len(), 1);
        let validated = VALIDATED_BYTES.with(std::cell::Cell::get);
        let scanned = chunk.len() + SCAN_OVERLAP_BYTES;
        assert!(
            validated <= scanned,
            "decoded {validated} bytes to scan {scanned}"
        );
    }

    fn chunk(registry: &mut WatchRegistry, bytes: &[u8], start: u64) -> Vec<(String, u64)> {
        let mut render = |text: &str, start: usize, end: usize, _: u64, _: u64| {
            (text[start..end].to_string(), String::new())
        };
        registry
            .scan_chunk("bash-1:stdout", "bash-1", bytes, start, false, &mut render)
            .into_iter()
            .map(|(hit, more)| (hit.match_text, more))
            .collect()
    }

    #[test]
    fn sticky_watch_counts_the_matches_its_notification_stands_for() {
        let mut registry = WatchRegistry::default();
        registry
            .register(
                "bash-1".into(),
                WatchPattern::Substring("READY".into()),
                false,
            )
            .unwrap();
        let three = b"READY 1\nREADY 2\nREADY 3\n";
        assert_eq!(
            chunk(&mut registry, three, 0),
            vec![("READY".to_string(), 2)]
        );
        assert_eq!(
            chunk(&mut registry, b"READY 4\n", three.len() as u64),
            vec![("READY".to_string(), 0)]
        );
        // A once watch ends with its first match; nothing is left unreported.
        let mut registry = regex_watch(r"READY \d", true);
        assert_eq!(
            chunk(&mut registry, three, 0),
            vec![("READY 1".to_string(), 0)]
        );
    }

    #[test]
    fn sticky_regex_holds_a_later_match_still_being_written() {
        let mut registry = regex_watch(r"port \d+", false);
        // The second value may still grow, so it is neither counted nor lost.
        let first = b"port 80\nport 44";
        assert_eq!(
            chunk(&mut registry, first, 0),
            vec![("port 80".to_string(), 0)]
        );
        assert_eq!(
            chunk(&mut registry, b"3\n", first.len() as u64),
            vec![("port 443".to_string(), 0)]
        );
    }
}
