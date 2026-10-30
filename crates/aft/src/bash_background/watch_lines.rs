//! Bounded, line-oriented delivery for opt-in asynchronous watches.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::watches::WatchPattern;

pub const MAX_LINES_PER_NOTIFICATION: usize = 50;
pub const MAX_NOTIFICATIONS_PER_MINUTE: usize = 12;
pub const NOTIFICATION_WINDOW: Duration = Duration::from_secs(60);
pub const MAX_LINE_BYTES: usize = 64 * 1024;
pub const MASKING_LINE_CAP_BYTES: usize = 8 * 1024;
pub const UNAVAILABLE_MATCH_SINGULAR: &str = "1 matching line over {limit} KiB was not shown; AFT can check lines only up to {limit} KiB for secrets.";
pub const UNAVAILABLE_MATCH_PLURAL: &str = "{count} matching lines over {limit} KiB were not shown; AFT can check lines only up to {limit} KiB for secrets.";
pub const UNAVAILABLE_PAGE_SINGULAR: &str = "1 line in this page over {limit} KiB was not shown; AFT can check lines only up to {limit} KiB for secrets.";
pub const UNAVAILABLE_PAGE_PLURAL: &str = "{count} lines in this page over {limit} KiB were not shown; AFT can check lines only up to {limit} KiB for secrets.";

pub fn unavailable_match_text(count: u64, limit: usize) -> String {
    unavailable_text(
        if count == 1 {
            UNAVAILABLE_MATCH_SINGULAR
        } else {
            UNAVAILABLE_MATCH_PLURAL
        },
        count,
        limit,
    )
}

pub(super) fn unavailable_page_text(count: u64) -> String {
    unavailable_text(
        if count == 1 {
            UNAVAILABLE_PAGE_SINGULAR
        } else {
            UNAVAILABLE_PAGE_PLURAL
        },
        count,
        MASKING_LINE_CAP_BYTES,
    )
}

fn unavailable_text(template: &str, count: u64, limit: usize) -> String {
    template
        .replace("{count}", &count.to_string())
        .replace("{limit}", &limit.div_ceil(1024).to_string())
}
pub const EACH_LINE_NEEDS_BACKGROUND: &str =
    "bash_watch: deliver: \"each_line\" needs background: true.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchLine {
    pub stream: String,
    pub offset: u64,
    pub end_offset: u64,
    pub text: String,
    pub count: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unavailable: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetryGap {
    pub reason: String,
    pub dropped_notifications: u64,
    pub known_missing_occurrences: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    #[serde(default)]
    pub evicted_through: u64,
    #[serde(default)]
    pub eviction_epoch: String,
    #[serde(default)]
    pub last_match_version: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LineBatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_gap: Option<RetryGap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub masking_limit_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub snapshot: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub terminal_summary: bool,
    pub lines: Vec<WatchLine>,
    pub dropped_lines: u64,
    pub first_dropped_offset: Option<u64>,
    pub first_dropped_stream: Option<String>,
    pub skipped_long_lines: u64,
}

impl LineBatch {
    pub(super) fn has_matches(&self) -> bool {
        !self.lines.is_empty() || self.dropped_lines > 0
    }

    pub fn merge(&mut self, next: LineBatch) {
        if next.retry_gap.is_some() {
            self.retry_gap = next.retry_gap.clone();
        }
        self.masking_limit_bytes = next.masking_limit_bytes.or(self.masking_limit_bytes);
        self.terminal_summary = next.terminal_summary;
        let mut retained = self.lines.iter().map(|line| line.count).sum::<u64>();
        for mut line in next.lines {
            let keep = line
                .count
                .min((MAX_LINES_PER_NOTIFICATION as u64).saturating_sub(retained));
            let dropped = line.count - keep;
            if dropped > 0 {
                let offset = line.offset + keep * (line.end_offset - line.offset);
                self.drop_line(&line.stream, offset);
                self.dropped_lines = self.dropped_lines.saturating_add(dropped - 1);
            }
            if keep > 0 {
                line.count = keep;
                self.lines.push(line);
                retained += keep;
            }
        }
        self.dropped_lines = self.dropped_lines.saturating_add(next.dropped_lines);
        self.skipped_long_lines = self
            .skipped_long_lines
            .saturating_add(next.skipped_long_lines);
        if self.first_dropped_offset.is_none() {
            self.first_dropped_offset = next.first_dropped_offset;
            self.first_dropped_stream = next.first_dropped_stream;
        }
    }

    fn drop_line(&mut self, stream: &str, offset: u64) {
        self.dropped_lines = self.dropped_lines.saturating_add(1);
        if self.first_dropped_offset.is_none() {
            self.first_dropped_offset = Some(offset);
            self.first_dropped_stream = Some(stream.to_string());
        }
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty()
            && self.dropped_lines == 0
            && self.skipped_long_lines == 0
            && self
                .retry_gap
                .as_ref()
                .is_none_or(|gap| gap.dropped_notifications == 0)
    }
}

#[derive(Debug, Default)]
struct PartialLine {
    bytes: Vec<u8>,
    offset: u64,
    oversized: bool,
}

#[derive(Debug, Default)]
pub(super) struct LineDelivery {
    streams: HashMap<String, PartialLine>,
    batch: LineBatch,
    retained: usize,
    window: Option<Instant>,
    sent: usize,
}

impl LineDelivery {
    pub fn replay_cursor(&self, stream: &str, cursor: u64) -> u64 {
        let partial = self
            .streams
            .get(stream)
            .filter(|line| !line.bytes.is_empty() || line.oversized)
            .map(|line| line.offset)
            .unwrap_or(cursor);
        let dropped = if self.batch.first_dropped_stream.as_deref() == Some(stream) {
            self.batch.first_dropped_offset.unwrap_or(cursor)
        } else {
            cursor
        };
        self.batch
            .lines
            .iter()
            .filter(|line| line.stream == stream)
            .map(|line| line.offset)
            .fold(cursor.min(partial).min(dropped), u64::min)
    }

    pub fn scan(&mut self, stream: &str, bytes: &[u8], base: u64, pattern: &WatchPattern) {
        let mut partial = self.streams.remove(stream).unwrap_or_else(|| PartialLine {
            offset: base,
            ..PartialLine::default()
        });
        let mut consumed = 0;
        for fragment in bytes.split_inclusive(|byte| *byte == b'\n') {
            let complete = fragment.last() == Some(&b'\n');
            let content = if complete {
                &fragment[..fragment.len() - 1]
            } else {
                fragment
            };
            if !partial.oversized {
                if partial.bytes.len().saturating_add(content.len()) > MAX_LINE_BYTES {
                    partial.bytes.clear();
                    partial.oversized = true;
                } else {
                    partial.bytes.extend_from_slice(content);
                }
            }
            consumed += fragment.len();
            if complete {
                self.finish_line(stream, &partial, base + consumed as u64, pattern);
                partial = PartialLine {
                    offset: base + consumed as u64,
                    ..PartialLine::default()
                };
            }
        }
        self.streams.insert(stream.to_string(), partial);
    }

    fn finish_line(
        &mut self,
        stream: &str,
        partial: &PartialLine,
        end: u64,
        pattern: &WatchPattern,
    ) {
        if partial.oversized {
            self.batch.skipped_long_lines = self.batch.skipped_long_lines.saturating_add(1);
            if self.batch.first_dropped_offset.is_none() {
                self.batch.first_dropped_offset = Some(partial.offset);
                self.batch.first_dropped_stream = Some(stream.to_string());
            }
            return;
        }
        let text = String::from_utf8_lossy(&partial.bytes);
        let matched = match pattern {
            WatchPattern::Substring(needle) => !needle.is_empty() && text.contains(needle),
            WatchPattern::Regex(regex) => regex.is_match(&text),
        };
        if !matched {
            return;
        }
        // Count source lines, not folded rows, so repetition cannot bypass the cap.
        if self.retained >= MAX_LINES_PER_NOTIFICATION {
            self.batch.drop_line(stream, partial.offset);
            return;
        }
        self.retained += 1;
        if let Some(previous) = self.batch.lines.last_mut() {
            if previous.stream == stream
                && previous.offset.saturating_add(
                    previous
                        .count
                        .saturating_mul(previous.end_offset - previous.offset),
                ) == partial.offset
                && previous.text == text
            {
                previous.count = previous.count.saturating_add(1);
                return;
            }
        }
        self.batch.lines.push(WatchLine {
            stream: stream.to_string(),
            offset: partial.offset,
            end_offset: end,
            text: text.into_owned(),
            count: 1,
            unavailable: false,
        });
    }

    pub fn take(
        &mut self,
        now: Instant,
        terminal: bool,
        pattern: &WatchPattern,
    ) -> Option<LineBatch> {
        if terminal {
            for (stream, partial) in std::mem::take(&mut self.streams) {
                if !partial.bytes.is_empty() || partial.oversized {
                    let end = partial.offset.saturating_add(partial.bytes.len() as u64);
                    self.finish_line(&stream, &partial, end, pattern);
                }
            }
        }
        if self
            .window
            .is_none_or(|start| now.saturating_duration_since(start) >= NOTIFICATION_WINDOW)
        {
            self.window = Some(now);
            self.sent = 0;
        }
        if self.sent >= MAX_NOTIFICATIONS_PER_MINUTE {
            for line in std::mem::take(&mut self.batch.lines) {
                self.batch.drop_line(&line.stream, line.offset);
                self.batch.dropped_lines = self.batch.dropped_lines.saturating_add(line.count - 1);
            }
            self.retained = 0;
            if !terminal {
                return None;
            }
            self.batch.terminal_summary = true;
        }
        if self.batch.is_empty() {
            return None;
        }
        self.sent += 1;
        self.retained = 0;
        Some(std::mem::take(&mut self.batch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern() -> WatchPattern {
        WatchPattern::Substring("ERROR".into())
    }

    #[test]
    fn each_line_delivers_all_matches_in_one_block() {
        for pattern in [pattern(), WatchPattern::regex("ERROR.*").unwrap()] {
            let mut delivery = LineDelivery::default();
            delivery.scan("stdout", b"ERROR a\nok\nERROR b\nERROR c\n", 0, &pattern);
            let batch = delivery.take(Instant::now(), false, &pattern).unwrap();
            assert_eq!(
                batch
                    .lines
                    .iter()
                    .map(|line| (line.offset, line.text.as_str()))
                    .collect::<Vec<_>>(),
                vec![(0, "ERROR a"), (11, "ERROR b"), (19, "ERROR c")]
            );
        }
    }

    #[test]
    fn each_line_split_across_blocks_delivered_once() {
        let mut delivery = LineDelivery::default();
        delivery.scan("stdout", b"ER", 40, &pattern());
        assert!(delivery.take(Instant::now(), false, &pattern()).is_none());
        delivery.scan("stdout", b"ROR a\n", 42, &pattern());
        let batch = delivery.take(Instant::now(), false, &pattern()).unwrap();
        assert_eq!(batch.lines[0].text, "ERROR a");
        assert_eq!(batch.lines[0].offset, 40);
        assert!(delivery.take(Instant::now(), true, &pattern()).is_none());
    }

    #[test]
    fn each_line_cap_reports_exact_count_and_offset() {
        let mut delivery = LineDelivery::default();
        for index in 0..53 {
            delivery.scan("stderr", b"ERROR\n", index * 6, &pattern());
        }
        let batch = delivery.take(Instant::now(), false, &pattern()).unwrap();
        assert_eq!(batch.lines.iter().map(|line| line.count).sum::<u64>(), 50);
        assert_eq!(batch.dropped_lines, 3);
        assert_eq!(batch.first_dropped_offset, Some(300));
        assert_eq!(batch.first_dropped_stream.as_deref(), Some("stderr"));
    }

    #[test]
    fn each_line_rate_cap_and_terminal_summary() {
        let now = Instant::now();
        let mut delivery = LineDelivery::default();
        for index in 0..12 {
            delivery.scan("stdout", b"ERROR\n", index * 6, &pattern());
            assert!(delivery.take(now, false, &pattern()).is_some());
        }
        delivery.scan("stdout", b"ERROR\nERROR\n", 72, &pattern());
        assert!(delivery.take(now, false, &pattern()).is_none());
        let batch = delivery.take(now, true, &pattern()).unwrap();
        assert_eq!(batch.dropped_lines, 2);
        assert_eq!(batch.first_dropped_offset, Some(72));
        assert!(batch.lines.is_empty());
        assert!(delivery
            .take(now + NOTIFICATION_WINDOW, false, &pattern())
            .is_none());
        delivery.scan("stdout", b"ERROR\n", 84, &pattern());
        assert_eq!(
            delivery
                .take(now + NOTIFICATION_WINDOW, false, &pattern())
                .unwrap()
                .lines
                .len(),
            1
        );
    }

    #[test]
    fn each_line_folds_only_consecutive_identical_lines() {
        let mut delivery = LineDelivery::default();
        delivery.scan(
            "stdout",
            b"ERROR a\nERROR a\nignore\nERROR a\n",
            0,
            &pattern(),
        );
        let batch = delivery.take(Instant::now(), false, &pattern()).unwrap();
        assert_eq!(batch.lines.len(), 2);
        assert_eq!(batch.lines[0].count, 2);
        assert_eq!(batch.lines[1].count, 1);
    }

    #[test]
    fn each_line_oversized_line_is_bounded_and_explicit() {
        let mut delivery = LineDelivery::default();
        delivery.scan("stdout", &vec![b'x'; MAX_LINE_BYTES], 0, &pattern());
        delivery.scan(
            "stdout",
            b"x\nERROR final",
            MAX_LINE_BYTES as u64,
            &pattern(),
        );
        let batch = delivery.take(Instant::now(), true, &pattern()).unwrap();
        assert_eq!(batch.skipped_long_lines, 1);
        assert_eq!(batch.first_dropped_offset, Some(0));
        assert_eq!(batch.lines[0].text, "ERROR final");
    }
}
