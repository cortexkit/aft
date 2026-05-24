use crate::compress::generic::GenericCompressor;
use crate::compress::{Compressor, Specificity};

pub struct BunCompressor;

/// Maximum number of failure blocks to preserve in a `bun test` run.
/// Beyond this, additional failures are dropped with a "+N more failures"
/// trailer so a catastrophic 1000-failure run still fits in the inline cap.
const MAX_FAILURES: usize = 25;

impl Compressor for BunCompressor {
    fn specificity(&self) -> Specificity {
        Specificity::PackageManager
    }

    fn matches(&self, command: &str) -> bool {
        command
            .split_whitespace()
            .next()
            .is_some_and(|head| head == "bun")
    }

    fn compress(&self, command: &str, output: &str) -> String {
        match bun_subcommand(command).as_deref() {
            Some("install" | "add" | "remove") => compress_package(output),
            Some("test") => compress_test(output),
            Some("run") => GenericCompressor::compress_output(output),
            Some("build") => compress_build(output),
            _ => GenericCompressor::compress_output(output),
        }
    }
}

fn bun_subcommand(command: &str) -> Option<String> {
    command
        .split_whitespace()
        .skip_while(|token| *token != "bun")
        .skip(1)
        .find(|token| !token.starts_with('-'))
        .map(ToString::to_string)
}

fn compress_package(output: &str) -> String {
    let mut result = Vec::new();
    for line in output.lines() {
        if is_bun_progress(line) {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.contains("packages installed")
            || trimmed.contains("package installed")
            || trimmed.starts_with("error:")
            || trimmed.starts_with("bun install error:")
            || trimmed.starts_with("Saved lockfile")
        {
            result.push(line.to_string());
        }
    }
    trim_trailing_lines(&result.join("\n"))
}

fn compress_build(output: &str) -> String {
    let mut result = Vec::new();
    let mut timing_seen = 0usize;
    let mut timing_omitted = 0usize;
    for line in output.lines() {
        if is_timing_line(line) {
            timing_seen += 1;
            if timing_seen > 10 {
                timing_omitted += 1;
                continue;
            }
        }
        result.push(line.to_string());
    }
    if timing_omitted > 0 {
        result.push(format!("... and {timing_omitted} more timing lines"));
    }
    trim_trailing_lines(&result.join("\n"))
}

/// Compress `bun test` output. Preserves:
///   - Bun version header (`bun test v1.3.14 ...`)
///   - File-section headers (`path/to/foo.test.ts:`) that precede a kept failure
///   - All failure context: `error:` block + diff + source pointer (`  N |`)
///     + stack frames + the explicit `(fail) test name [Xms]` marker
///   - Final summary lines: ` N pass`, ` N fail`, ` N expect() calls`, and
///     `Ran N tests across N files. [Xms]`
///
/// Drops:
///   - `(pass) test name [Xms]` markers (the summary's `N pass` line
///     already conveys count) — but if no failures exist, returns the
///     full original output via the generic compressor for safety
///
/// Cap: `MAX_FAILURES` failure blocks preserved; further blocks dropped
/// with a `+N more failures` trailer.
///
/// Why this matters: bun test writes failure blocks INLINE between the
/// header and the final summary. With the 30KB inline cap, large test
/// runs middle-truncate and lose the failure block entirely — agents
/// see only the header + summary count and have no debugging context.
fn compress_test(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return output.to_string();
    }

    // Quick pre-scan: if no failures, defer to generic. This keeps the
    // pass-only path cheap and avoids touching outputs that don't have
    // the truncation problem (small all-pass runs are already short).
    let has_failures = lines.iter().any(|line| is_bun_test_fail_marker(line));
    if !has_failures {
        return compress_test_pass_only(&lines);
    }

    let mut result: Vec<String> = Vec::new();
    let mut failures_kept = 0usize;
    let mut failures_dropped = 0usize;
    let mut index = 0usize;
    let mut saw_ran_summary = false;

    while index < lines.len() {
        let line = lines[index];

        if saw_ran_summary {
            // Past the `Ran N tests across M files. [Xms]` line, output
            // belongs to a chained command (`; next_cmd`). Pass through
            // so chains don't silently lose the next command's output.
            // (Note: `&&` short-circuits on test failure so this is mainly
            // relevant for `;` separators or `|| fallback_cmd`.)
            result.push(line.to_string());
            index += 1;
            continue;
        }

        // Bun version header — always keep.
        if is_bun_test_header(line) {
            result.push(line.to_string());
            index += 1;
            continue;
        }

        // File-section header (e.g. `src/foo.test.ts:`). Only keep if
        // a failure block follows before the next file-section header
        // or summary.
        if is_file_section_header(line) {
            let next_fail = next_index(&lines, index + 1, is_bun_test_fail_marker);
            let next_section = next_index(&lines, index + 1, |l| {
                is_file_section_header(l) || is_summary_line(l)
            });
            let keep_section = match (next_fail, next_section) {
                (Some(fi), Some(si)) => fi < si,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if keep_section {
                result.push(line.to_string());
            }
            index += 1;
            continue;
        }

        // Summary tail — always keep.
        if is_summary_line(line) {
            result.push(line.to_string());
            // The `Ran N tests across M files. [Xms]` line marks the
            // boundary between bun-test output and any chained-command
            // output that follows. (Bun uses `file. [` singular when
            // M == 1 and `files. [` plural otherwise.)
            if is_ran_summary_line(line) {
                saw_ran_summary = true;
            }
            index += 1;
            continue;
        }

        // Failure block: source pointers, error messages, diff lines,
        // stack frames, and the explicit `(fail) ...` marker.
        // Detect block start: an `error:` line, or a code-pointer block
        // (` N | ...`) that leads into an error.
        if is_bun_test_error_start(line) || is_bun_test_code_pointer(line) {
            // Collect lines up to and including the next `(fail) ...`
            // marker. We rely on the `(fail)` marker as the right edge
            // because bun always emits one per failed test after the
            // diagnostic block.
            let block_start = index;
            let mut block_end = index;
            while block_end < lines.len() {
                if is_bun_test_fail_marker(lines[block_end]) {
                    block_end += 1;
                    break;
                }
                // Stop early if we hit something that clearly isn't
                // part of a failure block — but be permissive: source
                // pointers, error text, stack frames, blank lines all
                // count as block content.
                block_end += 1;
            }

            failures_kept += 1;
            if failures_kept <= MAX_FAILURES {
                for line in &lines[block_start..block_end] {
                    result.push((*line).to_string());
                }
            } else {
                failures_dropped += 1;
            }
            index = block_end;
            continue;
        }

        // Drop everything else (individual pass lines, blank padding).
        index += 1;
    }

    if failures_dropped > 0 {
        result.push(format!("+{failures_dropped} more failures"));
    }

    // Safety net: if we somehow stripped everything, fall back so the
    // agent at least sees the raw bytes truncated by the generic path.
    if result.is_empty() {
        return GenericCompressor::compress_output(output);
    }
    trim_trailing_lines(&result.join("\n"))
}

/// All-pass `bun test` output: keep version header + summary + drop the
/// rest. Bun in default mode doesn't print per-test pass markers, but
/// `--verbose` does, so we explicitly preserve only header + summary.
///
/// IMPORTANT: when `bun test` is part of a shell chain like
/// `bun test && bun run build`, anything AFTER the
/// `Ran N tests across M files. [Xms]` line is the chained command's
/// output. We preserve those trailing lines unchanged so chains don't
/// silently lose the next command's output. The chained command itself
/// is generic content from our perspective (we have no signal about
/// what it is from the bun-test compressor's POV) so we pass it through
/// verbatim and let the inline cap handle excess size.
fn compress_test_pass_only(lines: &[&str]) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut saw_ran_summary = false;

    for line in lines {
        if saw_ran_summary {
            // Everything from here on is chained-command output. Pass through.
            result.push((*line).to_string());
            continue;
        }
        if is_bun_test_header(line) || is_summary_line(line) {
            result.push((*line).to_string());
            // The `Ran N tests across M files. [Xms]` line is the LAST line
            // bun emits for the test run itself. Everything after must be
            // from a chained command (`&& other_cmd`).
            if is_ran_summary_line(line) {
                saw_ran_summary = true;
            }
        }
    }

    if result.is_empty() {
        return GenericCompressor::compress_output(&lines.join("\n"));
    }
    trim_trailing_lines(&result.join("\n"))
}

fn next_index<F>(lines: &[&str], start: usize, predicate: F) -> Option<usize>
where
    F: Fn(&str) -> bool,
{
    lines
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, line)| predicate(line))
        .map(|(i, _)| i)
}

fn is_bun_test_header(line: &str) -> bool {
    line.starts_with("bun test v")
}

fn is_file_section_header(line: &str) -> bool {
    // File-section headers from bun look like `path/to/foo.test.ts:`
    // (no leading whitespace, no spaces in the path part, trailing
    // colon, contains `.test.` or `.spec.` to avoid false-positives on
    // error-message colons).
    let trimmed = line.trim_end();
    if trimmed.starts_with(' ') || !trimmed.ends_with(':') {
        return false;
    }
    let path = &trimmed[..trimmed.len() - 1];
    if path.is_empty() || path.contains(' ') {
        return false;
    }
    path.contains(".test.")
        || path.contains(".spec.")
        || path.contains("_test.")
        || path.contains("_spec.")
}

fn is_bun_test_fail_marker(line: &str) -> bool {
    line.trim_start().starts_with("(fail)")
}

fn is_bun_test_error_start(line: &str) -> bool {
    // Bun emits the failing assertion as `error: expect(...)` (and
    // sometimes plain `error: ...`). Source pointers preceding this
    // also need to be kept, but they get caught by the code-pointer
    // detector — this is the block's primary anchor.
    line.starts_with("error:")
}

fn is_bun_test_code_pointer(line: &str) -> bool {
    // Bun prints the failing line of source code with format `<N> | ...`
    // where <N> is the line number (with leading space to align).
    // These appear immediately above and below the `error:` line.
    let trimmed = line.trim_start();
    if !trimmed.contains(" | ") && !trimmed.contains("| ") {
        return false;
    }
    // Confirm it starts with a digit (line number).
    trimmed
        .chars()
        .next()
        .is_some_and(|char| char.is_ascii_digit())
}

/// Detects the `Ran N tests across M files. [Xms]` final line that bun
/// emits to mark the end of its own output. Accepts both the singular
/// (`file. [`) and plural (`files. [`) forms.
fn is_ran_summary_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("Ran ")
        && trimmed.contains(" tests")
        && (trimmed.contains(" files. [") || trimmed.contains(" file. ["))
}

fn is_summary_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    // Summary lines come after `[N]ms` markers in counts. Catch:
    //   " N pass", " N fail", " N expect() calls"
    //   "Ran N tests across N files. [Xms]"
    if trimmed.starts_with("Ran ") && trimmed.contains(" tests") {
        return true;
    }
    if let Some(first_token) = trimmed.split_whitespace().next() {
        if first_token.chars().all(|char| char.is_ascii_digit()) {
            let rest = trimmed[first_token.len()..].trim_start();
            return rest.starts_with("pass")
                || rest.starts_with("fail")
                || rest.starts_with("expect()");
        }
    }
    false
}

fn is_bun_progress(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "."
        || trimmed.chars().all(|char| char == '.')
        || trimmed.starts_with("Resolving")
        || trimmed.starts_with("Resolved")
        || trimmed.starts_with("Downloaded")
        || trimmed.starts_with("Extracted")
}

fn is_timing_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('[') && trimmed.contains(" ms]")
}

fn trim_trailing_lines(input: &str) -> String {
    input
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}
