use regex::{Regex, RegexBuilder};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use super::watch_lines::{LineBatch, LineDelivery};

const MAX_WATCHES_PER_TASK: usize = 8;
const CONTEXT_BEFORE: usize = 100;
const CONTEXT_AFTER: usize = 500;
const SCAN_OVERLAP_BYTES: usize = 8 * 1024;

pub const WATCH_TARGET_ERASED_TEXT: &str = "watch target erased";
pub const WATCH_TARGET_ERASED_CONTEXT: &str =
    "watch target erased: the background task row was erased before the watch reached a normal terminal result";
pub const WATCH_TASK_EXIT_TEXT: &str = "watch task exited";

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
    pub(super) fn copy_delivery_identity(&mut self, persisted: &Self) {
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
    }

    pub fn scan_chunk(
        &mut self,
        cursor_key: &str,
        task_id: &str,
        bytes: &[u8],
        start: u64,
        render: &mut dyn FnMut(&str, usize, usize, u64, u64) -> (String, String),
    ) -> Vec<PatternMatch> {
        self.scan_cursors.insert(
            cursor_key.to_string(),
            start.saturating_add(bytes.len() as u64),
        );
        self.scan_new_bytes_at(cursor_key, task_id, bytes, start, render)
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
            &mut |text, start, end, _, _| {
                (
                    text[start..end].to_string(),
                    context_snippet(text, start, end),
                )
            },
        )
    }

    fn scan_new_bytes_at(
        &mut self,
        cursor_key: &str,
        task_id: &str,
        bytes: &[u8],
        base_offset: u64,
        render: &mut dyn FnMut(&str, usize, usize, u64, u64) -> (String, String),
    ) -> Vec<PatternMatch> {
        let Some(watches) = self.watches.get(task_id).cloned() else {
            return Vec::new();
        };
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
        let scan_base_offset = base_offset.saturating_sub(prefix_len as u64);
        let mut matches = Vec::new();
        let mut remove_once = Vec::new();
        for watch in watches {
            if let Some(delivery) = self.line_delivery.get_mut(&watch.watch_id) {
                let stream = cursor_key.rsplit(':').next().unwrap_or("stdout");
                delivery.scan(stream, bytes, base_offset, &watch.pattern);
                continue;
            }
            if let Some((text_start, text_end, source_start, source_end)) =
                find_match(&watch.pattern, &text, &scan_bytes, prefix_len)
            {
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
                matches.push(PatternMatch {
                    watch_id: watch.watch_id.clone(),
                    task_id: watch.task_id.clone(),
                    match_text,
                    match_offset: scan_base_offset.saturating_add(source_start as u64),
                    context,
                    once: watch.once,
                    batch: None,
                    literal_pattern,
                });
                if watch.once {
                    remove_once.push(watch.watch_id);
                }
            }
        }
        for watch_id in remove_once {
            self.unregister(task_id, &watch_id);
        }
        let keep = scan_bytes.len().min(SCAN_OVERLAP_BYTES);
        self.scan_overlaps.insert(
            cursor_key.to_string(),
            scan_bytes[scan_bytes.len().saturating_sub(keep)..].to_vec(),
        );
        matches
    }
}

fn find_match(
    pattern: &WatchPattern,
    text: &str,
    source: &[u8],
    min_source_end_exclusive: usize,
) -> Option<(usize, usize, usize, usize)> {
    let make_match = |start: usize, end: usize| {
        let source_end = source_offset_for_lossy_utf8(source, end);
        (source_end > min_source_end_exclusive).then(|| {
            (
                start,
                end,
                source_offset_for_lossy_utf8(source, start),
                source_end,
            )
        })
    };
    match pattern {
        WatchPattern::Substring(needle) => {
            if needle.is_empty() {
                return None;
            }
            text.match_indices(needle)
                .find_map(|(start, matched)| make_match(start, start + matched.len()))
        }
        WatchPattern::Regex(regex) => regex
            .find_iter(text)
            .find_map(|matched| make_match(matched.start(), matched.end())),
    }
}

fn source_offset_for_lossy_utf8(source: &[u8], lossy_offset: usize) -> usize {
    let mut source_start = 0;
    let mut rendered_start = 0;
    while source_start < source.len() {
        match std::str::from_utf8(&source[source_start..]) {
            Ok(valid) => {
                return source_start
                    .saturating_add(lossy_offset.saturating_sub(rendered_start).min(valid.len()));
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                let rendered_valid_end = rendered_start.saturating_add(valid_len);
                if lossy_offset <= rendered_valid_end {
                    return source_start
                        .saturating_add(lossy_offset.saturating_sub(rendered_start));
                }
                source_start = source_start.saturating_add(valid_len);
                rendered_start = rendered_valid_end;

                let invalid_len = error
                    .error_len()
                    .unwrap_or_else(|| source.len().saturating_sub(source_start));
                if lossy_offset < rendered_start.saturating_add('\u{FFFD}'.len_utf8()) {
                    return source_start;
                }
                source_start = source_start.saturating_add(invalid_len);
                rendered_start = rendered_start.saturating_add('\u{FFFD}'.len_utf8());
            }
        }
    }
    source.len()
}

pub(super) fn context_snippet(text: &str, start: usize, end: usize) -> String {
    text[context_range(text, start, end)].replace('\r', "")
}

pub(super) fn context_range(text: &str, start: usize, end: usize) -> std::ops::Range<usize> {
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
}
