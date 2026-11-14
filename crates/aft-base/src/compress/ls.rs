use crate::compress::generic::{strip_ansi, GenericCompressor};
use crate::compress::listing_fold::{
    finish_folded, fold_consecutive_runs, shape_key_for_basename, FoldEntry,
};
use crate::compress::{CompressionResult, Compressor};

pub struct LsCompressor;

impl Compressor for LsCompressor {
    fn matches(&self, command: &str) -> bool {
        command_tokens(command).any(|token| token == "ls")
    }

    fn compress_with_exit_code(
        &self,
        command: &str,
        output: &str,
        _exit_code: Option<i64>,
    ) -> CompressionResult {
        let stripped = strip_ansi(output);
        if stripped.trim().is_empty() {
            return CompressionResult::new(stripped);
        }
        if is_ls_recursive(&stripped) {
            return GenericCompressor::compress_output(output).into();
        }
        let folded = compress_ls_listing(command, &stripped);
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

fn is_ls_recursive(output: &str) -> bool {
    output.lines().any(|line| {
        let t = line.trim_end();
        t.ends_with(':') && !t.starts_with("total ")
    })
}

fn compress_ls_listing(command: &str, output: &str) -> String {
    let long_format = command.split_whitespace().any(|t| {
        let t = t.trim_start_matches('-');
        t.contains('l')
    });

    let mut prefix = Vec::new();
    let mut entries = Vec::new();

    for line in output.lines() {
        if line.starts_with("total ") {
            prefix.push(line.to_string());
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        let Some((dir, basename)) = parse_ls_line(line, long_format) else {
            return output.to_string();
        };
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

    let mut folded = fold_consecutive_runs(entries);
    let mut out = prefix;
    out.append(&mut folded);
    finish_folded(out)
}

fn parse_ls_line(line: &str, long_format: bool) -> Option<(String, String)> {
    if long_format {
        let trimmed = line.trim_start();
        if trimmed.starts_with('-')
            || trimmed.starts_with('d')
            || trimmed.starts_with('l')
            || trimmed.starts_with('b')
            || trimmed.starts_with('c')
            || trimmed.starts_with('s')
            || trimmed.starts_with('p')
        {
            let name = line.split_whitespace().last()?.to_string();
            return Some((String::new(), name));
        }
        return None;
    }
    let name = line.trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some((String::new(), name))
}

pub fn build_lebench_ls_la_fixture() -> String {
    let mut lines = Vec::with_capacity(224);
    lines.push("total 1234".to_string());
    for i in 1..=200u32 {
        lines.push(format!(
            "-rw-r--r--  1 user staff  4096 Jan 01 00:00 module_{:03}.ts",
            i
        ));
    }
    // Needle sorts between module_100.ts and module_101.ts (index 101 after total).
    lines.insert(
        101,
        "-rw-r--r--  1 user staff  4096 Jan 01 00:00 module_100_NEEDLE_FILE_marker.ts".to_string(),
    );
    for i in 1..=22u32 {
        lines.push(format!(
            "-rw-r--r--  1 user staff  1024 Jan 01 00:00 filler_{:02}.log",
            i
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEEDLE: &str = "module_100_NEEDLE_FILE_marker.ts";

    #[test]
    fn matches_ls_invocations() {
        let c = LsCompressor;
        assert!(c.matches("ls -la src/generated/client"));
        assert!(c.matches("cd /tmp && ls"));
        assert!(!c.matches("gsl"));
    }

    #[test]
    fn lebench_224_line_ls_la_folds_and_preserves_needle() {
        let input = build_lebench_ls_la_fixture();
        let line_count = input.lines().count();
        assert_eq!(line_count, 224, "fixture line count");

        let out = compress_ls_listing("ls -la", &input);
        assert!(
            out.contains(NEEDLE),
            "needle must survive compression; got:\n{out}"
        );
        assert!(
            out.contains("module_*.ts"),
            "homogeneous run should fold to pattern summary"
        );
        assert!(
            out.lines().count() < 50,
            "should compress dramatically; got {} lines",
            out.lines().count()
        );
        eprintln!(
            "ls fixture: {} -> {} lines",
            line_count,
            out.lines().count()
        );
    }

    #[test]
    fn small_listing_passes_through_unchanged() {
        let input = (1..5)
            .map(|i| format!("-rw-r--r-- 1 u g 0 Jan 1 file_{i}.txt"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = compress_ls_listing("ls -l", &input);
        assert_eq!(out.lines().count(), 4);
        for i in 1..5 {
            assert!(out.contains(&format!("file_{i}.txt")));
        }
    }

    #[test]
    fn empty_ls_passes_through() {
        let c = LsCompressor;
        let r = c.compress_with_exit_code("ls", "", None);
        assert_eq!(r.text, "");
    }
}
