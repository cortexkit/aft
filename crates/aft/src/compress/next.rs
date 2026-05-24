use crate::compress::generic::{dedup_consecutive, middle_truncate, strip_ansi, GenericCompressor};
use crate::compress::Compressor;

const MAX_LINES: usize = 500;

pub struct NextCompressor;

impl Compressor for NextCompressor {
    fn matches(&self, command: &str) -> bool {
        let tokens: Vec<String> = command_tokens(command).collect();
        tokens.iter().any(|token| token == "next") && tokens.iter().any(|token| token == "build")
    }

    fn compress(&self, _command: &str, output: &str) -> String {
        compress_next(output)
    }
}

fn compress_next(output: &str) -> String {
    let stripped = strip_ansi(output);
    if has_build_error(&stripped) {
        return finish(&error_block(&stripped));
    }

    let extracted = extract_route_table(&stripped);
    if extracted.trim().is_empty() {
        return GenericCompressor::compress_output(&drop_noise(&stripped));
    }

    finish(&extracted)
}

fn command_tokens(command: &str) -> impl Iterator<Item = String> + '_ {
    command
        .split_whitespace()
        .map(|token| token.trim_matches(|ch| matches!(ch, '\'' | '"')))
        .map(|token| {
            token
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(token)
                .trim_end_matches(".cmd")
                .to_string()
        })
}

fn has_build_error(output: &str) -> bool {
    output.contains("Failed to compile")
        || output.contains("Type error:")
        || output.contains("SyntaxError:")
        || output
            .lines()
            .any(|line| line.trim_start().starts_with("Error:"))
}

fn error_block(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines
        .iter()
        .position(|line| is_error_line(line.trim_start()))
        .unwrap_or(0);
    lines[start..].join("\n")
}

fn is_error_line(trimmed: &str) -> bool {
    trimmed.contains("Failed to compile")
        || trimmed.starts_with("Error:")
        || trimmed.starts_with("Type error:")
        || trimmed.starts_with("SyntaxError:")
}

fn extract_route_table(output: &str) -> String {
    let mut kept = Vec::new();
    let mut in_table = false;
    let mut saw_table = false;

    for line in output.lines() {
        let trimmed = line.trim_start();
        if !in_table {
            if trimmed.starts_with("Route (") {
                in_table = true;
                saw_table = true;
                kept.push(line.to_string());
            }
            continue;
        }

        if should_preserve_route_line(trimmed) {
            kept.push(line.to_string());
        } else if saw_table && !trimmed.is_empty() {
            break;
        } else if trimmed.is_empty() {
            kept.push(line.to_string());
        }
    }

    trim_blank_edges(kept).join("\n")
}

fn should_preserve_route_line(trimmed: &str) -> bool {
    trimmed.starts_with("Route (")
        || trimmed.starts_with('┌')
        || trimmed.starts_with('├')
        || trimmed.starts_with('└')
        || trimmed.starts_with("+ First Load JS shared by all")
        || trimmed.starts_with("○  (")
        || trimmed.starts_with("ƒ  (")
        || trimmed.starts_with("●  (")
        || trimmed.starts_with("◐  (")
        || trimmed.starts_with("λ  (")
        || trimmed.starts_with("ƒ")
        || trimmed.starts_with("○")
        || trimmed.starts_with("●")
        || trimmed.starts_with("◐")
        || trimmed.starts_with("λ")
        || trimmed.starts_with('┬')
        || trimmed.starts_with('│')
        || trimmed.starts_with("└ ")
        || trimmed.starts_with("┌ ")
        || trimmed.starts_with("├ ")
        || trimmed.starts_with("└ other shared chunks")
        || trimmed.starts_with("├ chunks/")
        || trimmed.starts_with("└ chunks/")
        || trimmed.starts_with("├ ")
        || trimmed.starts_with("└ ")
}

fn drop_noise(output: &str) -> String {
    output
        .lines()
        .filter(|line| !is_noise_line(line.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_noise_line(trimmed: &str) -> bool {
    trimmed.starts_with("Attention: Next.js now collects")
        || trimmed.starts_with("Learn more:")
        || trimmed.starts_with("▲ Next.js")
        || trimmed.starts_with("Creating an optimized production build")
        || trimmed.starts_with("✓ ")
}

fn trim_blank_edges(lines: Vec<String>) -> Vec<String> {
    let start = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(start, |index| index + 1);
    lines[start..end].to_vec()
}

fn finish(input: &str) -> String {
    let deduped = dedup_consecutive(input);
    cap_lines(
        &middle_truncate(&deduped, 64 * 1024, 32 * 1024, 32 * 1024),
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

    fn sample_success_output() -> &'static str {
        r#"Attention: Next.js now collects completely anonymous telemetry...

▲ Next.js 15.2.0

   Creating an optimized production build ...
 ✓ Compiled successfully
 ✓ Linting and checking validity of types
 ✓ Collecting page data
 ✓ Generating static pages (12/12)
 ✓ Collecting build traces
 ✓ Finalizing page optimization

Route (app)                              Size     First Load JS
┌ ○ /                                    142 B          85.5 kB
├ ○ /_not-found                          885 B          82.5 kB
├ ○ /about                               142 B          85.5 kB
├ ƒ /api/health                          0 B                0 B
└ ƒ /dashboard                           1.2 kB         92.0 kB
+ First Load JS shared by all                            82.4 kB
  ├ chunks/2117-abc.js                   29.6 kB
  └ other shared chunks (total)          52.7 kB

○  (Static)   prerendered as static content
ƒ  (Dynamic)  server-rendered on demand
"#
    }

    #[test]
    fn matches_next_build_token_anywhere_and_rejects_substrings() {
        let compressor = NextCompressor;
        assert!(compressor.matches("next build"));
        assert!(compressor.matches("npx next build"));
        assert!(compressor.matches("pnpm exec next build"));
        assert!(compressor.matches("bun run next build"));
        assert!(compressor.matches("./node_modules/.bin/next build"));
        assert!(!compressor.matches("next dev"));
        assert!(!compressor.matches("next-i18n-router build"));
        assert!(!compressor.matches("pingnext build"));
    }

    #[test]
    fn happy_path_drops_noise_and_preserves_route_table() {
        let compressed = compress_next(sample_success_output());

        assert!(compressed.starts_with("Route (app)"));
        assert!(compressed.contains("└ ƒ /dashboard"));
        assert!(compressed.contains("+ First Load JS shared by all"));
        assert!(compressed.contains("○  (Static)"));
        assert!(compressed.contains("ƒ  (Dynamic)"));
        assert!(!compressed.contains("telemetry"));
        assert!(!compressed.contains("Next.js 15.2.0"));
        assert!(!compressed.contains("Compiled successfully"));
    }

    #[test]
    fn failure_path_preserves_error_block_verbatim() {
        let error_block = r#"Failed to compile.

./app/page.tsx:10:7
Type error: Type 'string' is not assignable to type 'number'.

   8 | export default function Page() {
   9 |   const count: number = 1;
> 10 |   const bad: number = "oops";
     |       ^
  11 |   return <main>{bad}</main>;
"#;
        let output = format!(
            "Attention: Next.js now collects completely anonymous telemetry...\n▲ Next.js 15.2.0\n ✓ Compiled successfully\n{error_block}"
        );

        let compressed = compress_next(&output);

        assert_eq!(compressed, error_block.trim_end());
        assert!(compressed.contains("Type error:"));
        assert!(!compressed.contains("Route (app)"));
    }

    #[test]
    fn large_route_table_compresses_below_half_and_keeps_markers() {
        let mut output = String::from(
            "Attention: Next.js now collects completely anonymous telemetry...\n▲ Next.js 15.2.0\n   Creating an optimized production build ...\n",
        );
        for index in 0..400 {
            output.push_str(&format!(" ✓ Progress step {index}\n"));
        }
        output.push_str("\nRoute (app)                              Size     First Load JS\n");
        output.push_str("┌ ○ /                                    142 B          85.5 kB\n");
        for index in 0..120 {
            output.push_str(&format!("├ ○ /page-{index:<30} 142 B          85.5 kB\n"));
        }
        output.push_str("└ ƒ /dashboard                           1.2 kB         92.0 kB\n");
        output.push_str("+ First Load JS shared by all                            82.4 kB\n  ├ chunks/2117-abc.js                   29.6 kB\n  └ other shared chunks (total)          52.7 kB\n\n○  (Static)   prerendered as static content\nƒ  (Dynamic)  server-rendered on demand\n");

        let compressed = compress_next(&output);

        assert!(compressed.len() < output.len() / 2);
        assert!(compressed.contains("Route (app)"));
        assert!(compressed.contains("/dashboard"));
        assert!(compressed.contains("ƒ  (Dynamic)"));
        assert!(!compressed.contains("Progress step"));
    }

    #[test]
    fn syntax_error_preserves_error_instead_of_extracting_table() {
        let output = r#"▲ Next.js 15.2.0
SyntaxError: Unexpected token '<'
    at app/page.tsx:1:1

Route (app)                              Size     First Load JS
┌ ○ /                                    142 B          85.5 kB
"#;

        let compressed = compress_next(output);

        assert!(compressed.starts_with("SyntaxError: Unexpected token '<'"));
        assert!(compressed.contains("at app/page.tsx:1:1"));
        assert!(compressed.contains("Route (app)"));
    }
}
