use crate::compress::generic::{dedup_consecutive, middle_truncate, strip_ansi};
use crate::compress::Compressor;

const MAX_LINES: usize = 250;

pub struct PrettierCompressor;

impl Compressor for PrettierCompressor {
    fn matches(&self, command: &str) -> bool {
        command_tokens(command).any(|token| token == "prettier")
    }

    fn compress(&self, _command: &str, output: &str) -> String {
        compress_prettier(output)
    }
}

fn compress_prettier(output: &str) -> String {
    let mut kept = Vec::new();
    let mut formatted = 0usize;
    let mut saw_diagnostic = false;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "Checking formatting..." {
            continue;
        }

        if trimmed.starts_with("[error]") {
            saw_diagnostic = true;
            kept.push(line.to_string());
            continue;
        }

        if trimmed.starts_with("[warn]") {
            saw_diagnostic = true;
            kept.push(line.to_string());
            continue;
        }

        if is_code_style_summary(trimmed) {
            saw_diagnostic = true;
            kept.push(line.to_string());
            continue;
        }

        if is_success_duration_line(trimmed) {
            if !trimmed.contains("(unchanged)") {
                formatted += 1;
            }
            continue;
        }

        kept.push(line.to_string());
    }

    if kept.is_empty() && (formatted > 0 || output.trim().is_empty()) {
        return format!("prettier: formatted {formatted} files");
    }

    if !saw_diagnostic && formatted > 0 && kept.is_empty() {
        return format!("prettier: formatted {formatted} files");
    }

    if kept.is_empty() {
        return format!("prettier: formatted {formatted} files");
    }

    finish(&kept.join("\n"))
}

fn command_tokens(command: &str) -> impl Iterator<Item = String> + '_ {
    command
        .split_whitespace()
        .map(|token| token.trim_matches(|ch| matches!(ch, '\'' | '"')))
        .filter(|token| !matches!(*token, "npx" | "pnpm" | "yarn" | "bun" | "bunx" | "exec"))
        .map(|token| {
            token
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(token)
                .trim_end_matches(".cmd")
                .to_string()
        })
}

fn is_code_style_summary(trimmed: &str) -> bool {
    trimmed.contains("Code style issues found")
}

fn is_success_duration_line(trimmed: &str) -> bool {
    let Some((_, tail)) = trimmed.rsplit_once(' ') else {
        return false;
    };
    let duration = tail
        .strip_suffix("ms")
        .or_else(|| tail.strip_suffix("ms (unchanged)"));
    if duration.is_some_and(|value| value.chars().all(|ch| ch.is_ascii_digit() || ch == '.')) {
        return true;
    }

    trimmed.ends_with("ms (unchanged)")
        && trimmed
            .rsplit_once(' ')
            .map(|(_, suffix)| suffix == "(unchanged)")
            .unwrap_or(false)
}

fn finish(input: &str) -> String {
    let stripped = strip_ansi(input);
    let deduped = dedup_consecutive(&stripped);
    cap_lines(
        &middle_truncate(&deduped, 32 * 1024, 16 * 1024, 16 * 1024),
        MAX_LINES,
    )
}

fn cap_lines(input: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = input.lines().collect();
    if lines.len() <= max_lines {
        return input.trim_end().to_string();
    }
    let mut kept = lines
        .iter()
        .take(max_lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    kept.push_str(&format!(
        "\n... truncated {} lines",
        lines.len() - max_lines
    ));
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_prettier_invocations() {
        let compressor = PrettierCompressor;
        assert!(compressor.matches("prettier --write src/"));
        assert!(compressor.matches("npx prettier --check"));
        assert!(compressor.matches("pnpm exec prettier"));
        assert!(compressor.matches("./node_modules/.bin/prettier --write ."));
        assert!(!compressor.matches("cargo build"));
        assert!(!compressor.matches("ls prettier.config.js"));
    }

    #[test]
    fn compresses_real_clean_format_pass() {
        let output = "src/foo.ts 25ms\nsrc/bar.ts 42ms (unchanged)\nsrc/baz.ts 18ms\n";
        let compressed = compress_prettier(output);
        assert_eq!(compressed, "prettier: formatted 2 files");
        assert!(!compressed.contains("src/foo.ts 25ms"));
        assert!(!compressed.contains("unchanged"));
        assert!(compressed.len() < output.len());
    }

    #[test]
    fn preserves_error_blocks_verbatim() {
        let output = "src/foo.ts 25ms\n[error] src/broken.ts: SyntaxError: Unexpected token (5:3)\n[error]   3 |\n[error] > 5 |   const x = ;\n[error]     |             ^\n";
        let compressed = compress_prettier(output);
        assert!(compressed.contains("[error] src/broken.ts: SyntaxError: Unexpected token (5:3)"));
        assert!(compressed.contains("[error]   3 |"));
        assert!(compressed.contains("[error] > 5 |   const x = ;"));
        assert!(compressed.contains("[error]     |             ^"));
        assert!(!compressed.contains("src/foo.ts 25ms"));
    }

    #[test]
    fn preserves_check_mode_warnings_and_summary() {
        let output = "Checking formatting...\n[warn] src/a.ts\n[warn] src/b.tsx\n[warn] Code style issues found in 2 files. Run Prettier with --write to fix.\n";
        let compressed = compress_prettier(output);
        assert!(compressed.contains("[warn] src/a.ts"));
        assert!(compressed.contains("[warn] src/b.tsx"));
        assert!(compressed.contains("Code style issues found in 2 files"));
        assert!(!compressed.contains("Checking formatting"));
    }

    #[test]
    fn compresses_large_success_input() {
        let mut output = String::new();
        for index in 0..500 {
            output.push_str(&format!("src/file{index}.ts {}ms\n", index + 1));
        }
        let compressed = compress_prettier(&output);
        assert!(compressed.contains("prettier: formatted 500 files"));
        assert!(compressed.len() < output.len() / 2);
        assert!(!compressed.contains("src/file499.ts"));
    }
}
