use crate::compress::generic::{strip_ansi, GenericCompressor};
use crate::compress::listing_fold::{
    finish_folded, fold_consecutive_runs, shape_key_for_basename, FoldEntry,
};
use crate::compress::{CompressionResult, Compressor};
use std::path::Path;

pub struct FindCompressor;

impl Compressor for FindCompressor {
    fn matches(&self, command: &str) -> bool {
        command_tokens(command).any(|token| token == "find")
    }

    fn compress_with_exit_code(
        &self,
        command: &str,
        output: &str,
        exit_code: Option<i64>,
    ) -> CompressionResult {
        let stripped = strip_ansi(output);
        if stripped.trim().is_empty() {
            if matches!(exit_code, Some(code) if code != 0) {
                return GenericCompressor::compress_output(output).into();
            }
            return CompressionResult::new("find: no matches");
        }
        let folded = compress_find_paths(command, &stripped);
        CompressionResult::new(folded)
    }
}

fn command_tokens(command: &str) -> impl Iterator<Item = String> + '_ {
    command
        .split_whitespace()
        .map(|token| token.trim_matches(|ch| matches!(ch, '\'' | '"')))
        .filter(|token| {
            !matches!(
                *token,
                "npx" | "pnpm" | "yarn" | "bun" | "bunx" | "exec" | "-m"
            )
        })
        .map(|token| {
            token
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(token)
                .trim_end_matches(".cmd")
                .to_string()
        })
}

fn compress_find_paths(_command: &str, output: &str) -> String {
    let mut entries = Vec::new();
    for line in output.lines() {
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        let path_obj = Path::new(path);
        let basename = path_obj
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string();
        let dir = path_obj
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .to_string();
        let shape_key = shape_key_for_basename(&dir, &basename);
        entries.push(FoldEntry {
            line: line.to_string(),
            dir,
            basename,
            shape_key,
        });
    }

    if entries.is_empty() {
        return output.trim_end().to_string();
    }

    let folded = fold_consecutive_runs(entries);
    finish_folded(folded)
}

pub fn build_lebench_find_fixture() -> String {
    let mut paths = Vec::with_capacity(223);
    for i in 1..=200u32 {
        paths.push(format!("src/generated/client/module_{:03}.ts", i));
    }
    paths.insert(
        100,
        "src/generated/client/module_100_NEEDLE_FILE_marker.ts".to_string(),
    );
    for i in 1..=22u32 {
        paths.push(format!("src/generated/client/extra_distinct_{i}.txt"));
    }
    paths.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEEDLE: &str = "module_100_NEEDLE_FILE_marker.ts";

    #[test]
    fn matches_find_invocations() {
        let c = FindCompressor;
        assert!(c.matches("find src -name '*.ts'"));
        assert!(!c.matches("findstr"));
    }

    #[test]
    fn find_no_matches_shortcircuit() {
        let c = FindCompressor;
        let r = c.compress_with_exit_code("find . -name missing", "", None);
        assert_eq!(r.text, "find: no matches");
    }

    #[test]
    fn lebench_find_folds_and_preserves_needle() {
        let input = build_lebench_find_fixture();
        let line_count = input.lines().count();

        let out = compress_find_paths("find", &input);
        assert!(out.contains(NEEDLE), "needle must survive; got:\n{out}");
        assert!(out.contains("module_*.ts"));
        assert!(out.lines().count() < line_count / 2);
        eprintln!(
            "find fixture: {} -> {} lines",
            line_count,
            out.lines().count()
        );
    }

    #[test]
    fn small_find_listing_unchanged() {
        let input = (1..5)
            .map(|i| format!("/tmp/small/file_{i}.txt"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = compress_find_paths("find", &input);
        assert_eq!(out.lines().count(), 4);
    }

    #[test]
    fn distinct_outliers_over_max_lines_middle_caps_with_note() {
        let marker = "MARKER_near_middle.txt";
        // Letter-only basenames so digit masking cannot collapse distinct entries.
        let mut paths: Vec<String> = (0..421)
            .map(|i| {
                let mut suffix = String::new();
                let mut n = i;
                for _ in 0..8 {
                    suffix.push((b'a' + (n % 26) as u8) as char);
                    n /= 26;
                }
                format!("/proj/outlier_{suffix}.rs")
            })
            .collect();
        paths.insert(410, format!("/proj/{marker}"));
        let input = paths.join("\n");
        let out = compress_find_paths("find", &input);
        assert!(
            out.contains("entries omitted"),
            "last-resort middle cap must be noted: {out}"
        );
        assert!(
            out.contains(marker),
            "marker within kept head/tail should survive: {out}"
        );
    }
}
