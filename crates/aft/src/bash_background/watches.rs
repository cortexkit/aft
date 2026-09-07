use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};

use super::persistence::ValidatedArtifact;

use regex::{Regex, RegexBuilder};
use serde::Serialize;

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
}

#[derive(Debug, Default)]
pub struct WatchRegistry {
    watches: HashMap<String, Vec<WatchSpec>>,
    scan_cursors: HashMap<String, u64>,
    scan_overlaps: HashMap<String, Vec<u8>>,
    controlled_tasks: HashSet<String>,
    matched_tasks: HashSet<String>,
    erased_notifications: HashSet<String>,
    next_watch: u64,
}

impl WatchRegistry {
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

    /// Drop in-memory watches whose ids are no longer durable (acked once-watches).
    pub fn retain_watch_ids(&mut self, task_id: &str, keep: &HashSet<String>) {
        if let Some(watches) = self.watches.get_mut(task_id) {
            watches.retain(|watch| keep.contains(&watch.watch_id));
            if watches.is_empty() {
                self.watches.remove(task_id);
            }
        }
    }

    pub fn clear_task(&mut self, task_id: &str) {
        self.watches.remove(task_id);
        self.controlled_tasks.remove(task_id);
        self.matched_tasks.remove(task_id);
        let prefix = format!("{task_id}:");
        self.scan_cursors
            .retain(|key, _| key != task_id && !key.starts_with(&prefix));
        self.scan_overlaps
            .retain(|key, _| key != task_id && !key.starts_with(&prefix));
    }

    pub fn terminalize_erased_task(&mut self, task_id: &str, watch_id: &str) -> bool {
        self.clear_task(task_id);
        self.erased_notifications
            .insert(format!("{task_id}:{watch_id}"))
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

    pub fn prime_file_cursor(&mut self, cursor_key: &str, file: &ValidatedArtifact) {
        if self.scan_cursors.contains_key(cursor_key) {
            return;
        }
        let len = file.len().unwrap_or(0);
        self.scan_cursors.insert(cursor_key.to_string(), len);
    }

    pub fn set_file_cursor(&mut self, cursor_key: &str, offset: u64) {
        self.scan_cursors.insert(cursor_key.to_string(), offset);
        self.scan_overlaps.remove(cursor_key);
    }

    pub fn scan_file_new_bytes(
        &mut self,
        cursor_key: &str,
        task_id: &str,
        file: &mut ValidatedArtifact,
    ) -> Vec<PatternMatch> {
        if self.active_count(task_id) == 0 {
            return Vec::new();
        }
        let cursor = self
            .scan_cursors
            .get(cursor_key)
            .copied()
            .unwrap_or_else(|| {
                // Start at current EOF so a newly registered watch does not match old spill content.
                file.len().unwrap_or(0)
            });
        if file.seek(SeekFrom::Start(cursor)).is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() || bytes.is_empty() {
            self.scan_cursors.insert(cursor_key.to_string(), cursor);
            return Vec::new();
        }
        let next = cursor.saturating_add(bytes.len() as u64);
        self.scan_cursors.insert(cursor_key.to_string(), next);
        self.scan_new_bytes_at(cursor_key, task_id, &bytes, cursor)
    }

    pub fn scan_new_bytes(&mut self, task_id: &str, bytes: &[u8]) -> Vec<PatternMatch> {
        let base = self.scan_cursors.get(task_id).copied().unwrap_or(0);
        self.scan_cursors
            .insert(task_id.to_string(), base.saturating_add(bytes.len() as u64));
        self.scan_new_bytes_at(task_id, task_id, bytes, base)
    }

    fn scan_new_bytes_at(
        &mut self,
        cursor_key: &str,
        task_id: &str,
        bytes: &[u8],
        base_offset: u64,
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
            if let Some((text_start, text_end, source_start, matched)) =
                find_match(&watch.pattern, &text, &scan_bytes, prefix_len)
            {
                self.matched_tasks.insert(task_id.to_string());
                matches.push(PatternMatch {
                    watch_id: watch.watch_id.clone(),
                    task_id: watch.task_id.clone(),
                    match_text: matched,
                    match_offset: scan_base_offset.saturating_add(source_start as u64),
                    context: context_snippet(&text, text_start, text_end),
                    once: watch.once,
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
) -> Option<(usize, usize, usize, String)> {
    let make_match = |start: usize, end: usize, matched: String| {
        let source_end = source_offset_for_lossy_utf8(source, end);
        (source_end > min_source_end_exclusive).then(|| {
            (
                start,
                end,
                source_offset_for_lossy_utf8(source, start),
                matched,
            )
        })
    };
    match pattern {
        WatchPattern::Substring(needle) => {
            if needle.is_empty() {
                return None;
            }
            text.match_indices(needle).find_map(|(start, matched)| {
                make_match(start, start + matched.len(), needle.clone())
            })
        }
        WatchPattern::Regex(regex) => regex.find_iter(text).find_map(|matched| {
            make_match(matched.start(), matched.end(), matched.as_str().to_string())
        }),
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

fn context_snippet(text: &str, start: usize, end: usize) -> String {
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
    text[before_start..after_end].replace('\r', "")
}

#[cfg(test)]
mod tests {
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
