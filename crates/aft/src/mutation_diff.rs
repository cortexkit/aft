//! Optional mutation diffs share a wire budget; file outcomes are never budgeted.

use std::io::{self, Write};

pub(crate) const DIFF_WIRE_BUDGET: usize = 8 * 1024 * 1024;
pub(crate) const DIFF_OMITTED_NOTICE: &str =
    "Diff omitted: too large to send. File results and line counts are complete.";

// Covers field names, quotes, commas and escaped newlines between joined patches.
const FIELD_OVERHEAD: usize = 64;

pub(crate) struct DiffBudget {
    remaining: usize,
    exhausted: bool,
}

impl Default for DiffBudget {
    fn default() -> Self {
        Self {
            remaining: DIFF_WIRE_BUDGET,
            exhausted: false,
        }
    }
}

impl DiffBudget {
    pub(crate) fn patch(
        &mut self,
        file: &str,
        before: &str,
        after: &str,
        copies: usize,
    ) -> Option<String> {
        self.render(before, after, copies, |out| {
            let diff = crate::edit::construct_diff(before, after);
            write_patch(out, file, before == after, &diff)
        })
    }

    pub(crate) fn patch_from_diff<'a>(
        &mut self,
        file: &str,
        before: &str,
        after: &str,
        diff: &'a similar::TextDiff<'a, 'a, 'a, str>,
        copies: usize,
    ) -> Option<String> {
        self.render(before, after, copies, |out| {
            write_patch(out, file, before == after, diff)
        })
    }

    pub(crate) fn contents(&mut self, before: &str, after: &str) -> bool {
        if self.exhausted {
            return false;
        }
        let cost = escaped_len(before)
            .saturating_add(escaped_len(after))
            .saturating_add(2 * FIELD_OVERHEAD);
        if cost > self.remaining {
            self.exhausted = true;
            return false;
        }
        self.remaining -= cost;
        true
    }

    fn render(
        &mut self,
        before: &str,
        after: &str,
        copies: usize,
        generate: impl FnOnce(&mut WireWriter) -> io::Result<()>,
    ) -> Option<String> {
        assert!(copies > 0);
        if self.exhausted {
            return None;
        }
        let limit = (self.remaining / copies).saturating_sub(FIELD_OVERHEAD);
        // A pure addition or deletion must send at least its escaped content.
        // Modified files can have tiny patches regardless of their source size.
        let pure_content = if before.is_empty() {
            Some(after)
        } else if after.is_empty() {
            Some(before)
        } else {
            None
        };
        if pure_content.is_some_and(|content| !escaped_fits(content, limit)) {
            return None;
        }
        let mut out = WireWriter {
            bytes: Vec::new(),
            escaped: 0,
            limit,
        };
        if generate(&mut out).is_err() {
            self.exhausted = true;
            return None;
        }
        let cost = (out.escaped + FIELD_OVERHEAD).saturating_mul(copies);
        if cost > self.remaining {
            self.exhausted = true;
            return None;
        }
        self.remaining -= cost;
        Some(String::from_utf8(out.bytes).expect("patch formatter writes UTF-8"))
    }
}

fn write_patch<'a>(
    out: &mut WireWriter,
    file: &str,
    identical: bool,
    diff: &'a similar::TextDiff<'a, 'a, 'a, str>,
) -> io::Result<()> {
    write!(
        out,
        "Index: {file}\n===================================================================\n"
    )?;
    if identical {
        write!(out, "--- {file}\n+++ {file}\n")
    } else {
        write!(out, "{}", diff.unified_diff().header(file, file))
    }
}

fn escaped_len(text: &str) -> usize {
    text.as_bytes()
        .iter()
        .map(|&byte| escaped_byte_len(byte))
        .sum()
}

fn escaped_fits(text: &str, limit: usize) -> bool {
    if text.len() > limit {
        return false;
    }
    let mut cost = 0;
    for &byte in text.as_bytes() {
        cost += escaped_byte_len(byte);
        if cost > limit {
            return false;
        }
    }
    true
}

fn escaped_byte_len(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
        0..=31 => 6,
        _ => 1,
    }
}

struct WireWriter {
    bytes: Vec<u8>,
    escaped: usize,
    limit: usize,
}

impl Write for WireWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut cost = self.escaped;
        for &byte in bytes {
            cost += escaped_byte_len(byte);
            if cost > self.limit {
                return Err(io::Error::other("optional diff wire budget exhausted"));
            }
        }
        self.bytes.extend_from_slice(bytes);
        self.escaped = cost;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn mark_omitted(result: &mut serde_json::Value) {
    result["diff_omitted"] = serde_json::json!(true);
    result["metadata"]["diff_omitted"] = serde_json::json!(true);
    result["diff_notice"] = serde_json::json!(DIFF_OMITTED_NOTICE);
    if let Some(output) = result
        .get("output")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
    {
        if !output.lines().any(|line| line == DIFF_OMITTED_NOTICE) {
            result["output"] = serde_json::json!(format!("{output}\n{DIFF_OMITTED_NOTICE}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bounded_mutation_diff_notice_is_pinned() {
        assert_eq!(
            DIFF_OMITTED_NOTICE,
            "Diff omitted: too large to send. File results and line counts are complete."
        );
    }

    #[test]
    fn bounded_mutation_diff_small_goldens() {
        for (before, after, tail) in [
            (
                "old\n",
                "new\n",
                "--- file.txt\n+++ file.txt\n@@ -1 +1 @@\n-old\n+new\n",
            ),
            (
                "",
                "new\n",
                "--- file.txt\n+++ file.txt\n@@ -0,0 +1 @@\n+new\n",
            ),
            (
                "old\n",
                "",
                "--- file.txt\n+++ file.txt\n@@ -1 +0,0 @@\n-old\n",
            ),
            ("same\n", "same\n", "--- file.txt\n+++ file.txt\n"),
        ] {
            let actual = DiffBudget::default()
                .patch("file.txt", before, after, 2)
                .unwrap();
            assert_eq!(actual, format!("Index: file.txt\n===================================================================\n{tail}"));
        }
    }

    #[test]
    fn bounded_mutation_diff_preflight_does_not_invoke_generator() {
        let mut budget = DiffBudget {
            remaining: 1024,
            exhausted: false,
        };
        for content in ["x".repeat(1000), "\0".repeat(200)] {
            for (before, after) in [("", content.as_str()), (content.as_str(), "")] {
                assert!(budget
                    .render(before, after, 2, |_| panic!(
                        "oversized pure diff reached generation"
                    ))
                    .is_none());
            }
        }
        // A large file need not hide another file's small, useful patch.
        assert!(budget.patch("small", "a\n", "b\n", 2).is_some());
    }

    #[test]
    fn bounded_mutation_diff_writer_stops_before_building_full_patch() {
        let mut budget = DiffBudget {
            remaining: 256,
            exhausted: false,
        };
        let mut visited = 0;
        assert!(budget
            .render("before", "after", 2, |out| {
                for _ in 0..1_000_000 {
                    visited += 1;
                    out.write_all(b"+\\\"\x00\n")?;
                    assert!(out.bytes.len() <= out.limit);
                }
                Ok(())
            })
            .is_none());
        assert!(visited < 10, "rendered {visited} lines before stopping");
        assert!(budget
            .render("before", "after", 1, |_| panic!(
                "exhausted budget generated another diff"
            ))
            .is_none());
    }

    #[test]
    fn bounded_mutation_diff_aggregate_counts_duplicates_unicode_and_json_escaping() {
        let mut budget = DiffBudget::default();
        let content = "é🦀\"\\\t\r\0\n".repeat(2000);
        let mut files = Vec::new();
        let mut patches = Vec::new();
        let mut omitted = 0;
        for index in 0..120 {
            if let Some(patch) = budget.patch("é\"\\.txt", "", &content, 2) {
                assert!(patch.contains("é🦀"));
                files.push(json!({"file": index, "patch": patch}));
                patches.push(patch);
            } else {
                files.push(json!({"file": index, "diff_omitted": true}));
                omitted += 1;
            }
        }
        assert!(omitted > 0 && omitted < 120);
        let wire =
            serde_json::to_vec(&json!({"files": files, "diff": patches.join("\n")})).unwrap();
        assert!(wire.len() <= DIFF_WIRE_BUDGET, "wire size {}", wire.len());
        assert_eq!(files.len(), 120);
        for text in ["é🦀\"\\\t\r\0\n", "\u{1}\u{8}\u{c}"] {
            assert_eq!(
                escaped_len(text),
                serde_json::to_string(text).unwrap().len() - 2
            );
        }
    }

    #[test]
    fn bounded_mutation_diff_preview_content_shares_patch_budget() {
        let mut budget = DiffBudget {
            remaining: 1024,
            exhausted: false,
        };
        assert!(budget.patch("file", "a\n", "b\n", 2).is_some());
        assert!(!budget.contents("", &"\0".repeat(200)));
        assert!(budget.patch("file", "a\n", "b\n", 2).is_none());
    }
}
