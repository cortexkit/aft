use crate::compress::caps::{cap_classified_blocks, ClassifiedBlock, DropClass};
use crate::compress::generic::GenericCompressor;
use crate::compress::{CompressionResult, Compressor, Specificity};

pub struct BunCompressor;

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

    fn compress_with_exit_code(
        &self,
        command: &str,
        output: &str,
        exit_code: Option<i64>,
    ) -> CompressionResult {
        match bun_subcommand(command).as_deref() {
            Some("install" | "i" | "add" | "remove") => compress_package(output).into(),
            Some("test") => preserve_bun_failure(output, compress_test(output), exit_code),
            Some("build") => compress_build(output),
            _ => GenericCompressor::compress_output(output).into(),
        }
    }

    fn matches_output(&self, output: &str) -> bool {
        let mut saw_ran_summary = false;
        let mut saw_result_marker = false;

        for line in output.lines() {
            saw_ran_summary |= is_ran_summary_line(line);
            saw_result_marker |= is_bun_test_result_marker(line);

            if saw_ran_summary && saw_result_marker {
                return true;
            }
        }

        false
    }

    fn compress_output_match_with_exit_code(
        &self,
        output: &str,
        exit_code: Option<i64>,
    ) -> CompressionResult {
        preserve_bun_failure(output, compress_test(output), exit_code)
    }
}

fn preserve_bun_failure(
    output: &str,
    compressed: CompressionResult,
    exit_code: Option<i64>,
) -> CompressionResult {
    let stripped_failure =
        compressed.text.trim().is_empty() || !super::text_has_failure_signal(&compressed.text);
    if !output.trim().is_empty()
        && stripped_failure
        && (matches!(exit_code, Some(code) if code != 0) || super::text_has_failure_signal(output))
    {
        GenericCompressor::compress_output(output).into()
    } else {
        compressed
    }
}

/// Known bun subcommands we want to match on. Used by `bun_subcommand`
/// to safely skip over flag values like `--cwd <dir>` that would
/// otherwise be misread as the subcommand. Listing only the
/// subcommands the compressor actually dispatches on plus the most
/// common bun verbs keeps the set small without missing real cases.
///
/// Full bun verb set (per `bun --help`): install, add, remove, update,
/// outdated, link, unlink, why, audit, patch, pm, publish, pack, run,
/// test, x, exec, create, init, build, repl, upgrade.
const BUN_SUBCOMMANDS: &[&str] = &[
    "install", "i", "add", "remove", "update", "outdated", "link", "unlink", "why", "audit",
    "patch", "pm", "publish", "pack", "run", "test", "x", "exec", "create", "init", "build",
    "repl", "upgrade", "help", "info",
];

/// Detect the bun subcommand from a command line.
///
/// Important: previous implementations used `find(!starts_with('-'))`
/// which broke for `bun --cwd packages/opencode-plugin test` — the
/// flag's value (`packages/opencode-plugin`) was returned as the
/// subcommand, causing the bun-test compressor to silently fall
/// through to the generic compressor and drop per-test failure
/// blocks. We now match against a whitelist of known bun verbs so
/// flag values are skipped safely.
fn bun_subcommand(command: &str) -> Option<String> {
    command
        .split_whitespace()
        .skip_while(|token| *token != "bun")
        .skip(1)
        .find(|token| BUN_SUBCOMMANDS.contains(token))
        .map(ToString::to_string)
}

fn compress_package(output: &str) -> String {
    let mut result = Vec::new();
    let mut last_content_line = None;
    for line in output.lines() {
        if is_bun_progress(line) {
            continue;
        }
        let trimmed = line.trim_start();
        if !trimmed.is_empty() {
            last_content_line = Some(line);
        }
        if trimmed.contains("packages installed")
            || trimmed.contains("package installed")
            || is_bun_install_summary(trimmed)
            || trimmed.starts_with("error:")
            || trimmed.starts_with("warn:")
            || trimmed.starts_with("bun install error:")
            || trimmed.starts_with("Saved lockfile")
        {
            result.push(line.to_string());
        }
    }
    // Never reduce a non-empty install to nothing: whatever bun printed last
    // is its verdict, and an empty reply hides whether the install ran at all.
    if result.is_empty() {
        if let Some(line) = last_content_line {
            result.push(line.to_string());
        }
    }
    trim_trailing_lines(&result.join("\n"))
}

/// Bun's closing summary when an install had nothing to add, e.g.
/// `Checked 437 installs across 463 packages (no changes) [18.00ms]`.
fn is_bun_install_summary(trimmed: &str) -> bool {
    trimmed.starts_with("Checked ") && trimmed.contains(" installs across ")
}

fn compress_build(output: &str) -> CompressionResult {
    let mut blocks = Vec::new();
    for line in output.lines() {
        if is_timing_line(line) {
            blocks.push(ClassifiedBlock::new(DropClass::Timing, line.to_string()));
        } else {
            blocks.push(ClassifiedBlock::unclassified(line.to_string()));
        }
    }
    let capped = cap_classified_blocks(blocks);
    CompressionResult::with_class_drops(trim_trailing_lines(&capped.text), capped.dropped_by_class)
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
/// Failure blocks are class-capped by the shared semantic cap helper; the
/// registry emits the single visible omission marker.
///
/// Why this matters: bun test writes failure blocks INLINE between the
/// header and the final summary. With the 30KB inline cap, large test
/// runs middle-truncate and lose the failure block entirely — agents
/// see only the header + summary count and have no debugging context.
fn compress_test(output: &str) -> CompressionResult {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return CompressionResult::new(output.to_string());
    }

    // Quick pre-scan: if no failures, defer to generic. This keeps the
    // pass-only path cheap and avoids touching outputs that don't have
    // the truncation problem (small all-pass runs are already short).
    let has_failures = lines.iter().any(|line| is_bun_test_fail_marker(line));
    if !has_failures {
        return CompressionResult::new(compress_test_pass_only(&lines));
    }

    let mut blocks: Vec<ClassifiedBlock> = Vec::new();
    let mut index = 0usize;
    let mut saw_ran_summary = false;
    let mut pending_section: Option<String> = None;

    while index < lines.len() {
        let line = lines[index];

        if saw_ran_summary {
            // Past the `Ran N tests across M files. [Xms]` line, output
            // belongs to a chained command (`; next_cmd`). Pass through
            // so chains don't silently lose the next command's output.
            // (Note: `&&` short-circuits on test failure so this is mainly
            // relevant for `;` separators or `|| fallback_cmd`.)
            blocks.push(ClassifiedBlock::unclassified(line.to_string()));
            index += 1;
            continue;
        }

        // Bun version header — keep, minus the noisy version + commit hash.
        if is_bun_test_header(line) {
            blocks.push(ClassifiedBlock::unclassified(render_bun_header(line)));
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
                pending_section = Some(line.to_string());
            }
            index += 1;
            continue;
        }

        // Summary tail — always keep (Ran line minus its [Xms] duration).
        if is_summary_line(line) {
            blocks.push(ClassifiedBlock::unclassified(render_summary_line(line)));
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

            let mut block_lines = Vec::new();
            if let Some(section) = pending_section.take() {
                block_lines.push(section);
            }
            block_lines.extend(
                lines[block_start..block_end]
                    .iter()
                    .map(|line| (*line).to_string()),
            );
            blocks.push(ClassifiedBlock::new(
                DropClass::Failure,
                block_lines.join("\n"),
            ));
            index = block_end;
            continue;
        }

        // Drop everything else (individual pass lines, blank padding).
        index += 1;
    }

    // Safety net: if we somehow stripped everything, fall back so the
    // agent at least sees the raw bytes truncated by the generic path.
    if blocks.is_empty() {
        return GenericCompressor::compress_output(output).into();
    }
    let capped = cap_classified_blocks(blocks);
    CompressionResult::with_class_drops(trim_trailing_lines(&capped.text), capped.dropped_by_class)
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
        if is_bun_test_header(line) {
            result.push(render_bun_header(line));
        } else if is_summary_line(line) {
            result.push(render_summary_line(line));
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

/// Render the bun banner without the noisy version + commit hash:
/// `bun test v1.3.14 (0d9b296a)` -> `bun test`. The version/hash is pure
/// per-call token tax with no agent value.
fn render_bun_header(line: &str) -> String {
    match line.find(" v") {
        Some(idx) => line[..idx].to_string(),
        None => line.to_string(),
    }
}

/// Strip the trailing ` [Xms]` wall-clock duration from the
/// `Ran N tests across M files. [Xms]` line — noise for the common case and
/// recoverable via `compressed:false`. Other summary lines (`N pass` etc.) and
/// non-Ran lines pass through unchanged.
fn render_summary_line(line: &str) -> String {
    if is_ran_summary_line(line.trim_start()) {
        if let Some(idx) = line.rfind(" [") {
            return line[..idx].to_string();
        }
    }
    line.to_string()
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

fn is_bun_test_result_marker(line: &str) -> bool {
    is_bun_test_pass_marker(line) || is_bun_test_fail_marker(line)
}

fn is_bun_test_pass_marker(line: &str) -> bool {
    is_bun_test_marker(line, "(pass)")
}

fn is_bun_test_fail_marker(line: &str) -> bool {
    is_bun_test_marker(line, "(fail)")
}

fn is_bun_test_marker(line: &str, marker: &str) -> bool {
    let trimmed = line.trim();
    let Some(rest) = trimmed.strip_prefix(marker) else {
        return false;
    };
    if !rest.chars().next().is_some_and(|ch| ch.is_whitespace()) {
        return false;
    }

    let name_and_timing = rest.trim_start();
    let Some((name, timing)) = name_and_timing.rsplit_once(" [") else {
        return false;
    };
    if name.trim().is_empty() {
        return false;
    }

    let Some(duration) = timing.strip_suffix(']') else {
        return false;
    };
    is_bun_test_duration(duration)
}

fn is_bun_test_duration(duration: &str) -> bool {
    ["ms", "µs", "μs", "us", "ns", "s"]
        .iter()
        .any(|unit| duration.strip_suffix(*unit).is_some_and(is_decimal_number))
}

fn is_decimal_number(value: &str) -> bool {
    let mut saw_digit = false;
    let mut saw_dot = false;

    for ch in value.chars() {
        match ch {
            '0'..='9' => saw_digit = true,
            '.' if !saw_dot => saw_dot = true,
            _ => return false,
        }
    }

    saw_digit
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
    let Some(rest) = line.strip_prefix("Ran ") else {
        return false;
    };
    let Some((test_count, rest)) = rest.split_once(" tests across ") else {
        return false;
    };
    if test_count.is_empty() || !test_count.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    let Some((file_count, rest)) = rest.split_once(" file") else {
        return false;
    };
    if file_count.is_empty() || !file_count.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    rest.starts_with(". [") || rest.starts_with("s. [")
}

fn is_summary_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    // Summary lines come after `[N]ms` markers in counts. Catch:
    //   " N pass", " N fail", " N expect() calls"
    //   "Ran N tests across N files. [Xms]"
    if is_ran_summary_line(trimmed) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bun_test_compile_error_falls_back_when_exit_is_unknown() {
        let output = "bun test v1.3.14 (0d9b296a)\n\nerror: Cannot find module './missing' from 'src/app.test.ts'\n\nRan 0 tests across 1 file. [12.00ms]\n";

        let compressed = BunCompressor.compress("bun test", output);

        assert!(compressed.text.contains("Cannot find module"));
        assert!(compressed.text.contains("Ran 0 tests"));
        assert_ne!(compressed.text, "bun test\nRan 0 tests across 1 file.");
    }

    #[test]
    fn bun_install_with_no_changes_keeps_its_summary() {
        let output = "bun install v1.4.2 (744846f84)\n\nChecked 437 installs across 463 packages (no changes) [18.00ms]\n";

        let compressed = BunCompressor.compress("bun install --frozen-lockfile", output);

        assert_eq!(
            compressed.text,
            "Checked 437 installs across 463 packages (no changes) [18.00ms]"
        );
    }

    #[test]
    fn bun_install_keeps_warnings_and_never_returns_empty() {
        let output = "bun install v1.4.2 (744846f84)\nwarn: incorrect peer dependency \"typebox@1.1.38\"\n\n+ typebox@1.1.38\n\n1 package installed [120.00ms]\n";
        let compressed = BunCompressor.compress("bun install", output);
        assert_eq!(
            compressed.text,
            "warn: incorrect peer dependency \"typebox@1.1.38\"\n1 package installed [120.00ms]"
        );

        // An unfamiliar closing line is still shown rather than dropped.
        let unfamiliar = "bun install v9.9.9\n\nDone in 4 steps\n";
        let compressed = BunCompressor.compress("bun install", unfamiliar);
        assert_eq!(compressed.text, "Done in 4 steps");
    }
}
