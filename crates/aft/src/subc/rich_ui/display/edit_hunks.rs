//! Structured diff hunks for edit display details.
use serde_json::{json, Value};
use similar::{ChangeTag, DiffTag};

use crate::patch::matcher::{self, NearestMissSearch};
use std::path::Path;

pub(crate) fn capture_no_match(path: &Path, source: &str, sought: &str, index: usize) {
    super::ACTIVE.with(|slot| {
        let active = slot.borrow();
        let Some(mut capture) = active.as_ref().and_then(|capture| capture.lock().ok()) else {
            return;
        };
        if capture.tool != "edit" {
            return;
        }
        let (mut record, _) = super::file_bodies(&capture, false);
        record["reason"] = json!("no_match");
        let detail = super::relative(&capture.root, path).and_then(|path| {
            let mut body = json!({
                "schema": "aft/files-detail/1",
                "revision": 1,
                "noMatchChange": index + 1,
                "path": path
            });
            no_match(source, sought).attach(&mut body);
            super::records::cut_detail(body, crate::edit::DIFF_RESPONSE_BUDGET)
        });
        capture.file_snapshot = Some((record, detail));
    });
}

const MAX_HUNKS: usize = 200;
const MAX_LINES: usize = 2_000;
const MAX_LINE_BYTES: usize = 4 * 1024;

#[derive(Default)]
pub(super) struct Hunks {
    pub(super) items: Vec<Value>,
    pub(super) truncated: bool,
    bytes: usize,
    lines: usize,
}

impl Hunks {
    fn push(&mut self, mut hunk: Value, lines: impl IntoIterator<Item = Value>) {
        if self.truncated {
            return;
        }
        let header_bytes = bytes(&hunk) + 1;
        if self.items.len() == MAX_HUNKS || !self.charge(header_bytes) {
            self.truncated = true;
            return;
        }
        let kept = hunk["lines"].as_array_mut().expect("hunk lines");
        for line in lines {
            if self.lines == MAX_LINES || !self.charge(bytes(&line) + 1) {
                self.truncated = true;
                break;
            }
            self.lines += 1;
            kept.push(line);
        }
        if !kept.is_empty() {
            self.items.push(hunk);
        }
    }

    fn charge(&mut self, bytes: usize) -> bool {
        if self.bytes + bytes > crate::edit::DIFF_RESPONSE_BUDGET {
            return false;
        }
        self.bytes += bytes;
        true
    }

    pub(super) fn attach(self, body: &mut Value) {
        if !self.items.is_empty() || self.truncated {
            body["hunks"] = json!(self.items);
        }
        if self.truncated {
            body["truncated"] = json!(true);
        }
    }
}

fn bytes(value: &Value) -> usize {
    serde_json::to_vec(value).expect("JSON value").len()
}

pub(super) fn diff(hunks: &mut Hunks, before: &str, after: &str) {
    let diff = crate::edit::construct_diff(before, after);
    let mut long_line = false;
    for group in diff.grouped_ops(3) {
        let updated = group.iter().filter(|op| op.tag() != DiffTag::Equal);
        let start = updated.clone().map(|op| op.new_range().start + 1).min();
        let end = updated.map(|op| op.new_range().end).max();
        let (Some(start), Some(end), Some(first)) = (start, end, group.first()) else {
            continue;
        };
        let number = hunks.items.len() + 1;
        let title = if end > start {
            format!("Change {number} · lines {start}–{end}")
        } else {
            format!("Change {number} · line {start}")
        };
        let lines = group
            .iter()
            .flat_map(|op| diff.iter_changes(op))
            .map(|change| {
                let kind = match change.tag() {
                    ChangeTag::Equal => "context",
                    ChangeTag::Delete => "delete",
                    ChangeTag::Insert => "add",
                };
                let raw = change.value().strip_suffix('\n').unwrap_or(change.value());
                let raw = raw.strip_suffix('\r').unwrap_or(raw);
                let text = line_prefix(raw);
                long_line |= text.len() < raw.len();
                json!({"kind":kind,"text":text})
            });
        hunks.push(json!({"title":title,"oldStart":first.old_range().start+1,"newStart":first.new_range().start+1,"lines":[]}), lines);
        if hunks.truncated {
            break;
        }
    }
    // A clipped text line still occupies its original number. Keep the
    // surrounding lines and mark the detail after building all its hunks.
    hunks.truncated |= long_line;
}

fn line_prefix(text: &str) -> &str {
    let mut end = text.len().min(MAX_LINE_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn no_match(source: &str, sought: &str) -> Hunks {
    let mut hunks = Hunks::default();
    let found = if source.len() <= matcher::NEAREST_MISS_MAX_FILE_BYTES
        && sought.len() <= matcher::NEAREST_MISS_MAX_FILE_BYTES
    {
        let lines: Vec<_> = source.lines().collect();
        let pattern: Vec<_> = sought.lines().collect();
        match matcher::find_nearest_miss(&lines, &pattern, source.len()) {
            NearestMissSearch::Found(candidate) => Some((
                candidate.start + 1,
                lines[candidate.start..candidate.end].join("\n"),
            )),
            _ => None,
        }
    } else {
        None
    };
    let mut clipped = false;
    let mut other = found
        .as_ref()
        .into_iter()
        .flat_map(|(_, text)| text.lines());
    hunks.push(
        json!({"title":"Looked for","lines":[]}),
        sought
            .lines()
            .map(|line| plain(line, other.next(), &mut clipped)),
    );
    if let Some((start, text)) = found {
        let mut other = sought.lines();
        hunks.push(
            json!({"title":format!("Closest text in the file · line {start}"),"lines":[]}),
            text.split('\n')
                .map(|line| plain(line, Some(other.next().unwrap_or_default()), &mut clipped)),
        );
    }
    hunks.truncated |= clipped;
    hunks
}

fn plain(line: &str, other: Option<&str>, clipped: &mut bool) -> Value {
    let text = line_prefix(line);
    *clipped |= text.len() < line.len();
    let mut result = json!({"kind":"plain","text":text});
    if let Some(other) = other.map(line_prefix).filter(|other| *other != text) {
        let start = text
            .chars()
            .zip(other.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let len = text.chars().count();
        let other_len = other.chars().count();
        let suffix = text
            .chars()
            .rev()
            .zip(other.chars().rev())
            .take((len - start).min(other_len - start))
            .take_while(|(a, b)| a == b)
            .count();
        result["marks"] = json!([{"start":start,"end":len-suffix}]);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_hunks_include_three_numbered_context_lines_on_each_side() {
        let mut hunks = Hunks::default();
        diff(
            &mut hunks,
            "0\n1\n2\n3\nold\n5\n6\n7\n8\n",
            "0\n1\n2\n3\nnew\nadded\n5\n6\n7\n8\n",
        );
        assert!(!hunks.truncated);
        assert_eq!(
            hunks.items,
            [
                json!({"title":"Change 1 · lines 5–6","oldStart":2,"newStart":2,"lines":[
                    {"kind":"context","text":"1"}, {"kind":"context","text":"2"}, {"kind":"context","text":"3"},
                    {"kind":"delete","text":"old"}, {"kind":"add","text":"new"}, {"kind":"add","text":"added"},
                    {"kind":"context","text":"5"}, {"kind":"context","text":"6"}, {"kind":"context","text":"7"}
                ]})
            ]
        );
    }

    #[test]
    fn edit_hunks_keep_a_visible_prefix_within_the_escaped_byte_budget() {
        let mut hunks = Hunks::default();
        let after = "\"é".repeat(1_000) + "\n";
        diff(&mut hunks, "old\n", &after.repeat(100));
        assert!(hunks.truncated);
        let mut detail = json!({"schema":"aft/files-detail/1"});
        hunks.attach(&mut detail);
        let limit = crate::edit::DIFF_RESPONSE_BUDGET;
        let detail = super::super::records::cut_detail(detail, limit).unwrap();
        assert!(bytes(&detail) <= limit);
        assert_eq!(detail["truncated"], true);
        let lines = detail["hunks"][0]["lines"].as_array().unwrap();
        assert_eq!(lines[0], json!({"kind":"delete","text":"old"}));
        assert_eq!(lines[1]["text"], after.trim_end());
        assert!(lines.len() < 101);
    }
}
