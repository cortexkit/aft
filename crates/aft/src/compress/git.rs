use crate::compress::generic::{dedup_consecutive, middle_truncate, GenericCompressor};
use crate::compress::{CompressionResult, Compressor};

const STATUS_SHORT_LIMIT: usize = 1024;
const STATUS_KEEP_PER_SECTION: usize = 10;
const DIFF_MAX_FILES: usize = 5;
const DIFF_MAX_HUNKS: usize = 20;
const HUNK_KEEP_LINES: usize = 30;
const LOG_SHORT_HASH_LEN: usize = 12;
const BLAME_KEEP_LINES: usize = 50;
const GIT_WRITE_KEEP_LINES: usize = 50;
const GIT_ADD_KEEP_PATHS: usize = 5;
const GIT_STASH_STATUS_KEEP_LINES: usize = 20;

pub struct GitCompressor;

impl Compressor for GitCompressor {
    fn matches(&self, command: &str) -> bool {
        command_head(command).is_some_and(|head| head == "git")
    }

    fn compress_with_exit_code(
        &self,
        command: &str,
        output: &str,
        exit_code: Option<i32>,
    ) -> CompressionResult {
        let compressed = match git_subcommand(command).as_deref() {
            Some("add") => compress_add(output),
            Some("status") => compress_status(output),
            Some("diff") => compress_diff(output, false),
            Some("log") => compress_log(output),
            Some("show") => compress_diff(output, true),
            Some("branch") => trim_trailing_lines(&dedup_consecutive(output)),
            Some("blame") => compress_blame(output),
            Some("commit") => compress_commit(output),
            Some("push") => compress_push(output),
            Some("pull") => compress_pull(output),
            Some("fetch") => compress_fetch(output),
            Some("stash") => compress_stash(command, output),
            _ => GenericCompressor::compress_output(output),
        };
        if matches!(exit_code, Some(code) if code != 0)
            && looks_like_git_success_summary(&compressed)
        {
            GenericCompressor::compress_output(output).into()
        } else {
            compressed.into()
        }
    }
}

fn looks_like_git_success_summary(text: &str) -> bool {
    let trimmed = text.trim();
    matches!(
        trimmed,
        "git: ok" | "git fetch: ok" | "Everything up-to-date" | "Already up to date."
    ) || trimmed.contains("working tree clean")
}

fn command_head(command: &str) -> Option<&str> {
    command.split_whitespace().next()
}

fn git_subcommand(command: &str) -> Option<String> {
    let mut seen_git = false;
    for token in command.split_whitespace() {
        if !seen_git {
            if token == "git" {
                seen_git = true;
            }
            continue;
        }
        if token.starts_with('-') || token.contains('=') {
            continue;
        }
        if crate::compress::is_shell_boundary(token) {
            return None;
        }
        return Some(token.to_string());
    }
    None
}

fn git_subcommand_after(command: &str, subcommand: &str) -> Option<String> {
    let mut seen_git = false;
    let mut seen_subcommand = false;
    for token in command.split_whitespace() {
        if !seen_git {
            if token == "git" {
                seen_git = true;
            }
            continue;
        }
        if !seen_subcommand {
            if token.starts_with('-') || token.contains('=') {
                continue;
            }
            seen_subcommand = token == subcommand;
            continue;
        }
        if token.starts_with('-') || token.contains('=') {
            continue;
        }
        if crate::compress::is_shell_boundary(token) {
            return None;
        }
        return Some(token.to_string());
    }
    None
}

fn compress_add(output: &str) -> String {
    if output.trim().is_empty() {
        return "git: ok".to_string();
    }
    if looks_like_git_error(output) {
        return trim_trailing_lines(output);
    }
    let lines: Vec<&str> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return "git: ok".to_string();
    }
    let mut result: Vec<String> = lines
        .iter()
        .take(GIT_ADD_KEEP_PATHS)
        .map(|line| line.trim_end().to_string())
        .collect();
    if lines.len() > GIT_ADD_KEEP_PATHS {
        result.push(format!(
            "... ({} more files added)",
            lines.len() - GIT_ADD_KEEP_PATHS
        ));
    }
    cap_git_lines(result, "files added", GIT_WRITE_KEEP_LINES)
}

fn compress_commit(output: &str) -> String {
    if output.trim().is_empty() {
        return GenericCompressor::compress_output(output);
    }
    if looks_like_git_error(output) {
        return trim_trailing_lines(output);
    }
    if let Some(line) = output
        .lines()
        .find(|line| line.contains("nothing to commit"))
    {
        return line.trim_end().to_string();
    }
    let subject = output.lines().find(|line| looks_like_commit_subject(line));
    let summary = output.lines().find(|line| looks_like_commit_summary(line));
    match (subject, summary) {
        (Some(subject), Some(summary)) => {
            trim_trailing_lines(&format!("{}\n{}", subject.trim_end(), summary.trim()))
        }
        (Some(subject), None) => subject.trim_end().to_string(),
        _ => GenericCompressor::compress_output(output),
    }
}

fn compress_push(output: &str) -> String {
    if output.trim().is_empty() {
        return GenericCompressor::compress_output(output);
    }
    if looks_like_git_error(output) {
        return trim_trailing_lines(output);
    }
    if let Some(line) = output
        .lines()
        .find(|line| line.trim() == "Everything up-to-date")
    {
        return line.trim_end().to_string();
    }
    let result: Vec<String> = output
        .lines()
        .filter(|line| is_remote_destination(line) || is_ref_update_line(line))
        .map(|line| line.trim_end().to_string())
        .collect();
    if result.is_empty() {
        return GenericCompressor::compress_output(output);
    }
    cap_git_lines(result, "push lines", GIT_WRITE_KEEP_LINES)
}

fn compress_pull(output: &str) -> String {
    if output.trim().is_empty() {
        return GenericCompressor::compress_output(output);
    }
    if looks_like_git_error(output) {
        return trim_trailing_lines(output);
    }
    if let Some(line) = output
        .lines()
        .find(|line| line.trim() == "Already up to date.")
    {
        return line.trim_end().to_string();
    }
    let result: Vec<String> = output
        .lines()
        .filter(|line| {
            looks_like_updating_line(line)
                || looks_like_pull_marker(line)
                || looks_like_commit_summary(line)
        })
        .map(|line| line.trim_end().to_string())
        .collect();
    if result.is_empty() {
        return GenericCompressor::compress_output(output);
    }
    cap_git_lines(result, "pull lines", GIT_WRITE_KEEP_LINES)
}

fn compress_fetch(output: &str) -> String {
    if output.trim().is_empty() {
        return "git fetch: ok".to_string();
    }
    if looks_like_git_error(output) {
        return trim_trailing_lines(output);
    }
    let result: Vec<String> = output
        .lines()
        .filter(|line| is_fetch_from_line(line) || is_ref_update_line(line))
        .map(|line| line.trim_end().to_string())
        .collect();
    if result.is_empty() {
        return GenericCompressor::compress_output(output);
    }
    cap_git_lines(result, "fetch lines", GIT_WRITE_KEEP_LINES)
}

fn compress_stash(command: &str, output: &str) -> String {
    if output.trim().is_empty() {
        return GenericCompressor::compress_output(output);
    }
    if looks_like_git_error(output) {
        return trim_trailing_lines(output);
    }
    match git_subcommand_after(command, "stash").as_deref() {
        None | Some("push") | Some("save") => output
            .lines()
            .find(|line| line.starts_with("Saved working directory and index state"))
            .map(|line| line.trim_end().to_string())
            .unwrap_or_else(|| GenericCompressor::compress_output(output)),
        Some("pop" | "apply") => cap_git_lines(
            output
                .lines()
                .map(|line| line.trim_end().to_string())
                .collect(),
            "stash status lines",
            GIT_STASH_STATUS_KEEP_LINES,
        ),
        Some("list") => trim_trailing_lines(output),
        _ => GenericCompressor::compress_output(output),
    }
}

fn looks_like_git_error(output: &str) -> bool {
    output.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("error:")
            || trimmed.starts_with("fatal:")
            || trimmed.starts_with("CONFLICT ")
            || trimmed.starts_with("Automatic merge failed")
            || trimmed.starts_with("! [rejected]")
            || trimmed.starts_with("! [remote rejected]")
            || trimmed.starts_with("failed to push")
    })
}

fn looks_like_commit_subject(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('[') && trimmed.contains("] ")
}

fn looks_like_commit_summary(line: &str) -> bool {
    let trimmed = line.trim();
    (trimmed.contains("file changed") || trimmed.contains("files changed"))
        && (trimmed.contains("insertion")
            || trimmed.contains("deletion")
            || trimmed.contains("changed"))
}

fn is_remote_destination(line: &str) -> bool {
    line.starts_with("To ")
}

fn is_fetch_from_line(line: &str) -> bool {
    line.starts_with("From ")
}

fn is_ref_update_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.contains(" -> ")
        && (trimmed.starts_with('*')
            || trimmed.starts_with('+')
            || trimmed.starts_with('-')
            || trimmed.starts_with('=')
            || trimmed.starts_with('!')
            || trimmed.split_whitespace().next().is_some_and(is_hash_range))
}

fn is_hash_range(token: &str) -> bool {
    token
        .split_once("..")
        .is_some_and(|(left, right)| is_short_hash(left) && is_short_hash(right))
}

fn is_short_hash(token: &str) -> bool {
    (4..=40).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn looks_like_updating_line(line: &str) -> bool {
    line.trim_start().starts_with("Updating ")
}

fn looks_like_pull_marker(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "Fast-forward" || trimmed.starts_with("Merge made by ")
}

fn cap_git_lines(mut lines: Vec<String>, summary_name: &str, keep_lines: usize) -> String {
    if lines.len() > keep_lines {
        let omitted = lines.len() - keep_lines;
        lines.truncate(keep_lines);
        lines.push(format!("... ({} more {})", omitted, summary_name));
    }
    trim_trailing_lines(&lines.join("\n"))
}

fn compress_status(output: &str) -> String {
    if output.len() <= STATUS_SHORT_LIMIT {
        return trim_trailing_lines(output);
    }

    let mut result = Vec::new();
    let mut section_entries = Vec::new();
    let mut in_section = false;

    for line in output.lines() {
        if is_status_section_header(line) {
            flush_status_entries(&mut result, &mut section_entries);
            result.push(line.to_string());
            in_section = true;
        } else if in_section && is_status_instructional(line) {
            // Lines like `  (use "git add <file>..." to include in what will be
            // committed)` come right after the section header in real git
            // output. They're informational, not entries — pass them through
            // verbatim WITHOUT resetting `in_section` so the entries that
            // follow still get aggregated and summarized.
            result.push(line.to_string());
        } else if in_section && is_status_entry(line) {
            section_entries.push(line.to_string());
        } else {
            flush_status_entries(&mut result, &mut section_entries);
            result.push(line.to_string());
            in_section = false;
        }
    }
    flush_status_entries(&mut result, &mut section_entries);

    trim_trailing_lines(&result.join("\n"))
}

fn is_status_section_header(line: &str) -> bool {
    matches!(
        line.trim_end_matches(':'),
        "Changes to be committed"
            | "Changes not staged for commit"
            | "Untracked files"
            | "Unmerged paths"
    )
}

/// Recognize the parenthesized instructional lines git emits inside a status
/// section, e.g. `  (use "git add <file>..." to include in what will be committed)`.
/// These come right after the section header and must NOT reset the
/// in-section state, otherwise the actual entries that follow are missed by
/// the entry aggregator.
fn is_status_instructional(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('(') || trimmed.starts_with("use ")
}

fn is_status_entry(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("modified:")
        || trimmed.starts_with("new file:")
        || trimmed.starts_with("deleted:")
        || trimmed.starts_with("renamed:")
        || trimmed.starts_with("copied:")
        || trimmed.starts_with("both modified:")
        || trimmed.starts_with("both added:")
        || trimmed.starts_with("deleted by us:")
        || trimmed.starts_with("deleted by them:")
        || (!trimmed.is_empty()
            && !trimmed.starts_with('(')
            && !trimmed.starts_with("use ")
            && !trimmed.starts_with("no changes"))
}

fn flush_status_entries(result: &mut Vec<String>, entries: &mut Vec<String>) {
    if entries.is_empty() {
        return;
    }

    let keep = entries.len().min(STATUS_KEEP_PER_SECTION);
    result.extend(entries.iter().take(keep).cloned());
    if entries.len() > keep {
        result.push(format!("... and {} more", entries.len() - keep));
    }
    entries.clear();
}

fn compress_diff(output: &str, keep_commit_header: bool) -> String {
    let files = split_diff_files(output, keep_commit_header);
    let total_hunks: usize = files.iter().map(|file| count_hunks(&file.lines)).sum();

    if files.is_empty() || total_hunks <= 2 && output.len() <= 5 * 1024 {
        return trim_trailing_lines(output);
    }

    let max_files = if total_hunks > DIFF_MAX_HUNKS {
        DIFF_MAX_FILES
    } else {
        usize::MAX
    };

    let mut result = Vec::new();
    let mut emitted_files = 0usize;

    for file in &files {
        if file.is_diff && emitted_files >= max_files {
            continue;
        }
        result.extend(compress_diff_file(&file.lines));
        emitted_files += usize::from(file.is_diff);
    }

    let changed_files = files.iter().filter(|file| file.is_diff).count();
    if changed_files > emitted_files {
        result.push(format!(
            "... and {} more files changed",
            changed_files - emitted_files
        ));
    }

    middle_truncate(
        &trim_trailing_lines(&result.join("\n")),
        16 * 1024,
        7 * 1024,
        7 * 1024,
    )
}

struct DiffFile {
    lines: Vec<String>,
    is_diff: bool,
}

fn split_diff_files(output: &str, keep_commit_header: bool) -> Vec<DiffFile> {
    let mut files = Vec::new();
    let mut current = Vec::new();
    let mut current_is_diff = false;

    for line in output.lines() {
        if line.starts_with("diff --git ") {
            if !current.is_empty() {
                files.push(DiffFile {
                    lines: std::mem::take(&mut current),
                    is_diff: current_is_diff,
                });
            }
            current_is_diff = true;
        } else if !current_is_diff && !keep_commit_header && !line.starts_with("diff --git ") {
            current_is_diff = true;
        }
        current.push(line.to_string());
    }

    if !current.is_empty() {
        files.push(DiffFile {
            lines: current,
            is_diff: current_is_diff,
        });
    }

    files
}

fn compress_diff_file(lines: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line = &lines[index];
        if !line.starts_with("@@") {
            result.push(line.clone());
            index += 1;
            continue;
        }

        let hunk_start = index;
        index += 1;
        while index < lines.len() && !lines[index].starts_with("@@") {
            index += 1;
        }
        let hunk = &lines[hunk_start..index];
        append_hunk(&mut result, hunk);
    }

    result
}

fn append_hunk(result: &mut Vec<String>, hunk: &[String]) {
    if hunk.len() <= HUNK_KEEP_LINES + 1 {
        result.extend(hunk.iter().cloned());
        return;
    }

    result.extend(hunk.iter().take(HUNK_KEEP_LINES + 1).cloned());
    let remaining = &hunk[HUNK_KEEP_LINES + 1..];
    let added = remaining
        .iter()
        .filter(|line| line.starts_with('+'))
        .count();
    let removed = remaining
        .iter()
        .filter(|line| line.starts_with('-'))
        .count();
    result.push(format!(
        "... +{} -{} in {} more lines",
        added,
        removed,
        remaining.len()
    ));
}

fn count_hunks(lines: &[String]) -> usize {
    lines.iter().filter(|line| line.starts_with("@@")).count()
}

fn compress_log(output: &str) -> String {
    if output.lines().any(|line| line.starts_with("commit ")) {
        compress_full_format_log(output)
    } else {
        compress_oneline_log(output)
    }
}

fn compress_full_format_log(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let mut blocks: Vec<usize> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("commit ") {
            blocks.push(index);
        }
    }
    if blocks.is_empty() {
        return trim_trailing_lines(output);
    }

    let mut result = Vec::new();
    for (block_index, &start) in blocks.iter().enumerate() {
        let end = blocks.get(block_index + 1).copied().unwrap_or(lines.len());
        let block = &lines[start..end];
        if let Some(compact) = format_log_commit_block(block) {
            result.push(compact);
        }
    }

    trim_trailing_lines(&result.join("\n"))
}

fn format_log_commit_block(block: &[&str]) -> Option<String> {
    let first = block.first()?;
    let full_hash = first.strip_prefix("commit ")?.trim();
    let short_hash = abbreviate_log_hash(full_hash);

    let mut merge_parents: Option<String> = None;
    let mut author: Option<String> = None;
    let mut date_compact: Option<String> = None;
    let mut header_end = 1usize;

    for (offset, line) in block.iter().enumerate().skip(1) {
        if line.starts_with("Merge: ") {
            let rest = line.strip_prefix("Merge: ").unwrap_or("");
            let parents: Vec<String> = rest.split_whitespace().map(abbreviate_log_hash).collect();
            merge_parents = Some(format!("Merge: {}", parents.join(" ")));
            header_end = offset + 1;
        } else if let Some(rest) = line.strip_prefix("Author: ") {
            author = Some(format_author_compact(rest.trim()));
            header_end = offset + 1;
        } else if let Some(rest) = line.strip_prefix("Date: ") {
            date_compact = Some(compact_git_log_date(rest.trim()));
            header_end = offset + 1;
        } else if line.trim().is_empty() {
            header_end = offset + 1;
            break;
        } else {
            break;
        }
    }

    let mut body_iter = block[header_end..].iter().copied();
    let mut subject = String::new();
    let mut body_rest = Vec::new();
    if let Some(first_body) = body_iter.next() {
        subject = first_body.trim().to_string();
        for line in body_iter {
            body_rest.push(normalize_log_body_line(line));
        }
    }

    let author = author.unwrap_or_default();
    let date_compact = date_compact.unwrap_or_default();
    let mut header = format!("{short_hash} {subject}");
    if let Some(merge) = merge_parents {
        header = format!("{short_hash} {merge} {subject}");
    }
    if !author.is_empty() {
        header.push_str(&format!("  {author}"));
    }
    if !date_compact.is_empty() {
        header.push(' ');
        header.push_str(&date_compact);
    }

    let mut out = header;
    for line in collapse_log_body_blank_runs(&body_rest) {
        out.push('\n');
        out.push_str(&line);
    }
    Some(out)
}

fn normalize_log_body_line(line: &str) -> String {
    if line.starts_with(' ') || line.starts_with('\t') {
        format!("    {}", line.trim_start())
    } else {
        format!("    {line}")
    }
}

fn collapse_log_body_blank_runs(body: &[String]) -> Vec<String> {
    body.iter()
        .filter(|line| !line.trim().is_empty())
        .cloned()
        .collect()
}

fn format_author_compact(author: &str) -> String {
    if let Some((name, email)) = author.split_once('<') {
        let name = name.trim();
        let email = email.trim_end_matches('>').trim();
        format!("<{name} {email}>")
    } else {
        format!("<{author}>")
    }
}

fn abbreviate_log_hash(hash: &str) -> String {
    let hash = hash.trim();
    if hash.len() <= LOG_SHORT_HASH_LEN {
        hash.to_string()
    } else {
        hash[..LOG_SHORT_HASH_LEN].to_string()
    }
}

fn compact_git_log_date(date_field: &str) -> String {
    let parts: Vec<&str> = date_field.split_whitespace().collect();
    if parts.len() < 5 {
        return date_field.to_string();
    }
    let month = parts[1];
    let day = parts[2];
    let time = parts[3];
    let year = parts[4];
    let month_num = match month {
        "Jan" => "01",
        "Feb" => "02",
        "Mar" => "03",
        "Apr" => "04",
        "May" => "05",
        "Jun" => "06",
        "Jul" => "07",
        "Aug" => "08",
        "Sep" => "09",
        "Oct" => "10",
        "Nov" => "11",
        "Dec" => "12",
        _ => return date_field.to_string(),
    };
    let day_padded = if day.len() == 1 {
        format!("0{day}")
    } else {
        day.to_string()
    };
    format!("{year}-{month_num}-{day_padded} {time}")
}

fn compress_oneline_log(output: &str) -> String {
    let result: Vec<String> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            if looks_like_oneline_commit(line) {
                let mut parts = line.splitn(2, ' ');
                let hash = parts.next().unwrap_or("");
                let rest = parts.next().unwrap_or("");
                let short = abbreviate_log_hash(hash);
                if rest.is_empty() {
                    short
                } else {
                    format!("{short} {rest}")
                }
            } else {
                line.trim_end().to_string()
            }
        })
        .collect();
    trim_trailing_lines(&result.join("\n"))
}

fn looks_like_oneline_commit(line: &str) -> bool {
    let Some((hash, _message)) = line.split_once(' ') else {
        return false;
    };
    (7..=40).contains(&hash.len()) && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn compress_blame(output: &str) -> String {
    let total = output.lines().count();
    if total <= BLAME_KEEP_LINES {
        return trim_trailing_lines(output);
    }

    let mut result: Vec<String> = output
        .lines()
        .take(BLAME_KEEP_LINES)
        .map(ToString::to_string)
        .collect();
    result.push(format!("... {} more blame lines", total - BLAME_KEEP_LINES));
    result.join("\n")
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
    use crate::compress::Compressor;

    fn compress(command: &str, output: &str) -> CompressionResult {
        GitCompressor.compress(command, output)
    }

    #[test]
    fn test_add_empty_output_ok() {
        let compressed = compress("git add .", "");
        assert_eq!(compressed, "git: ok");
    }
    #[test]
    fn test_add_verbose_many_files() {
        let raw = "add 'src/a.rs'\nadd 'src/b.rs'\nadd 'src/c.rs'\nadd 'src/d.rs'\nadd 'src/e.rs'\nadd 'src/f.rs'\nadd 'src/g.rs'\n";
        let compressed = compress("git add --verbose .", raw);
        assert!(compressed.contains("add 'src/a.rs'"));
        assert!(compressed.contains("add 'src/e.rs'"));
        assert!(compressed.contains("... (2 more files added)"));
        assert!(!compressed.contains("add 'src/g.rs'"));
    }
    #[test]
    fn test_add_error_passthrough() {
        let raw = "fatal: pathspec 'missing.rs' did not match any files\n";
        let compressed = compress("git add missing.rs", raw);
        assert_eq!(
            compressed,
            "fatal: pathspec 'missing.rs' did not match any files"
        );
    }
    #[test]
    fn test_commit_success_extracts_subject_and_summary() {
        let raw = "[main 1a2b3c4] add git write compression\n 3 files changed, 42 insertions(+), 7 deletions(-)\n create mode 100644 crates/aft/src/foo.rs\n rewrite crates/aft/src/bar.rs (80%)\n";
        let compressed = compress("git commit -m 'add git write compression'", raw);
        assert_eq!(
            compressed,
            "[main 1a2b3c4] add git write compression\n3 files changed, 42 insertions(+), 7 deletions(-)"
        );
    }
    #[test]
    fn test_commit_nothing_to_commit_verbatim() {
        let raw = "On branch main\nnothing to commit, working tree clean\n";
        let compressed = compress("git commit -m noop", raw);
        assert_eq!(compressed, "nothing to commit, working tree clean");
    }
    #[test]
    fn test_commit_error_passthrough() {
        let raw = "error: Committing is not possible because you have unmerged files.\nhint: Fix them up in the work tree, and then use 'git add/rm <file>'\nfatal: Exiting because of an unresolved conflict.\n";
        let compressed = compress("git commit", raw);
        assert!(compressed.contains("error: Committing is not possible"));
        assert!(compressed.contains("fatal: Exiting because of an unresolved conflict."));
    }
    #[test]
    fn test_push_success_drops_progress_keeps_remote_and_ref() {
        let raw = "Counting objects: 12, done.\nDelta compression using up to 8 threads\nCompressing objects: 100% (7/7), done.\nWriting objects: 100% (7/7), 1.23 KiB | 1.23 MiB/s, done.\nTotal 7 (delta 4), reused 0 (delta 0), pack-reused 0\nremote: Resolving deltas: 100% (4/4), completed with 4 local objects.\nTo github.com:example/repo.git\n   9d8c7b6..1a2b3c4  main -> main\n";
        let compressed = compress("git push", raw);
        assert_eq!(
            compressed,
            "To github.com:example/repo.git\n   9d8c7b6..1a2b3c4  main -> main"
        );
    }
    #[test]
    fn test_push_everything_up_to_date_and_empty() {
        assert_eq!(
            compress("git push", "Everything up-to-date\n"),
            "Everything up-to-date"
        );
        assert_eq!(compress("git push", ""), "");
    }
    #[test]
    fn test_push_error_passthrough() {
        let raw = "To github.com:example/repo.git\n ! [rejected]        main -> main (fetch first)\nerror: failed to push some refs to 'github.com:example/repo.git'\n";
        let compressed = compress("git push", raw);
        assert!(compressed.contains("! [rejected]        main -> main (fetch first)"));
        assert!(compressed.contains("error: failed to push some refs"));
    }
    #[test]
    fn test_pull_fast_forward_keeps_summary() {
        let raw = "remote: Enumerating objects: 9, done.\nremote: Counting objects: 100% (9/9), done.\nFrom github.com:example/repo\n   1111111..2222222  main       -> origin/main\nUpdating 1111111..2222222\nFast-forward\n crates/aft/src/compress/git.rs | 12 +++++++++---\n 1 file changed, 9 insertions(+), 3 deletions(-)\n";
        let compressed = compress("git pull --ff-only", raw);
        assert_eq!(
            compressed,
            "Updating 1111111..2222222\nFast-forward\n 1 file changed, 9 insertions(+), 3 deletions(-)"
        );
    }
    #[test]
    fn test_pull_already_up_to_date_empty_and_error() {
        assert_eq!(
            compress("git pull", "Already up to date.\n"),
            "Already up to date."
        );
        assert_eq!(compress("git pull", ""), "");
        let raw = "CONFLICT (content): Merge conflict in README.md\nAutomatic merge failed; fix conflicts and then commit the result.\n";
        let compressed = compress("git pull", raw);
        assert!(compressed.contains("CONFLICT (content): Merge conflict in README.md"));
        assert!(compressed.contains("Automatic merge failed"));
    }
    #[test]
    fn test_fetch_success_empty_and_error() {
        let raw = "remote: Enumerating objects: 5, done.\nremote: Counting objects: 100% (5/5), done.\nFrom github.com:example/repo\n * [new branch]      feature/git-compress -> origin/feature/git-compress\n   abc1234..def5678  main                 -> origin/main\n";
        let compressed = compress("git fetch --all", raw);
        assert_eq!(
            compressed,
            "From github.com:example/repo\n * [new branch]      feature/git-compress -> origin/feature/git-compress\n   abc1234..def5678  main                 -> origin/main"
        );
        assert_eq!(compress("git fetch", "   \n"), "git fetch: ok");
        let error =
            "fatal: unable to access 'https://example.invalid/repo.git/': Could not resolve host\n";
        assert_eq!(
            compress("git fetch", error),
            "fatal: unable to access 'https://example.invalid/repo.git/': Could not resolve host"
        );
    }
    #[test]
    fn test_stash_push_pop_list_empty_and_error() {
        let push = "Saved working directory and index state WIP on main: 1a2b3c4 add tests\nHEAD is now at 1a2b3c4 add tests\n";
        assert_eq!(
            compress("git stash push", push),
            "Saved working directory and index state WIP on main: 1a2b3c4 add tests"
        );
        let pop = "On branch main\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n\tmodified:   README.md\nDropped refs/stash@{0} (abc123456789)\n";
        let compressed_pop = compress("git stash pop", pop);
        assert!(compressed_pop.contains("On branch main"));
        assert!(compressed_pop.contains("Dropped refs/stash@{0}"));
        let list = "stash@{0}: WIP on main: 1111111 first\nstash@{1}: On feature: second\n";
        assert_eq!(compress("git stash list", list), list.trim_end());
        assert_eq!(compress("git stash", ""), "");
        let error = "error: Your local changes to the following files would be overwritten by merge:\n\tREADME.md\n";
        let compressed_error = compress("git stash apply", error);
        assert!(compressed_error.contains("error: Your local changes"));
        assert!(compressed_error.contains("README.md"));
    }

    #[test]
    fn test_log_merge_commit_keeps_parents() {
        let raw = "commit cccccccccccccccccccccccccccccccccccc\nMerge: dddddddddddddddddddddddddddddddddddd eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\nAuthor: Merger <merge@example.com>\nDate:   Mon Jan 01 12:00:00 2024 +0000\n\n    Merge branch 'feature'\n";
        let compressed = compress("git log", raw);
        assert!(compressed.contains("Merge: dddddddddddd eeeeeeeeeeee"));
        assert!(compressed.contains("Merge branch 'feature'"));
    }

    #[test]
    fn test_log_format_collapse_short_log() {
        let raw = "commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nAuthor: Alice Example <alice@example.com>\nDate:   Thu Jun 18 17:39:12 2026 +0200\n\n    first subject\n    detail one\n\ncommit bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nAuthor: Bob Example <bob@example.com>\nDate:   Wed Jun 17 10:00:00 2026 +0000\n\n    second subject\n";
        let compressed = compress("git log", raw);
        assert!(!compressed.contains("commit "));
        assert!(!compressed.contains("Author:"));
        assert!(!compressed.contains("Date:"));
        assert!(compressed.contains("aaaaaaaaaaaa first subject"));
        assert!(compressed.contains("<Alice Example alice@example.com>"));
        assert!(compressed.contains("2026-06-18 17:39:12"));
        assert!(compressed.contains("detail one"));
        assert!(compressed.contains("bbbbbbbbbbbb second subject"));
        assert!(!compressed.contains(" ago"));
        assert!(compressed.len() < raw.len());
    }

    #[test]
    fn test_log_compress_is_deterministic() {
        let raw = "commit 1111111111111111111111111111111111111111\nAuthor: x <x@y.com>\nDate:   Thu Jun 18 17:39:12 2026 +0200\n\n    subject\n    body\n";
        let a = compress_log(raw);
        let b = compress_log(raw);
        assert_eq!(a, b);
        assert!(a.contains("111111111111"));
        assert!(a.contains("2026-06-18 17:39:12"));
    }

    #[test]
    fn test_log_oneline_abbreviates_hash_keeps_all_lines() {
        let raw = "e4e8f7e1234567890abcdef1234567890abcdef (HEAD -> main) chore\n9c4aa18abcdef1234567890abcdef1234567890 feat\n";
        let compressed = compress("git log --oneline", raw);
        assert!(compressed.contains("e4e8f7e12345"));
        assert!(compressed.contains("(HEAD -> main) chore"));
        assert!(compressed.contains("9c4aa18abcde"));
        assert!(compressed.contains("feat"));
        assert!(!compressed.contains("... more commits"));
    }

    #[test]
    fn test_log_deep_needle_survives_without_drop_line() {
        let raw = include_str!("../../tests/fixtures/git_log_deep_needle.txt");
        let compressed = compress("git log", raw);
        assert!(compressed.contains("feedfacefeed"));
        assert!(compressed.contains("NEEDLE_GIT_auth_bypass"));
        assert!(compressed.contains("UNIQUE_BODY_MARKER_needle_xyz"));
        assert!(!compressed.contains("... more commits"));
        assert!(compressed.len() < raw.len());
    }

    #[test]
    fn git_subcommand_returns_none_for_pipe_before_subcommand() {
        assert_eq!(git_subcommand("git --no-pager | grep log"), None);
    }

    #[test]
    fn git_subcommand_returns_subcommand_when_before_pipe() {
        assert_eq!(git_subcommand("git log | grep fix").as_deref(), Some("log"));
    }

    #[test]
    fn git_subcommand_returns_none_for_redirect_before_subcommand() {
        assert_eq!(git_subcommand("git --no-pager > out.log"), None);
    }

    #[test]
    fn git_subcommand_unaffected_without_metacharacters() {
        assert_eq!(git_subcommand("git log --oneline").as_deref(), Some("log"));
    }

    #[test]
    fn git_subcommand_after_returns_none_for_pipe() {
        assert_eq!(git_subcommand_after("git stash | grep outputs", "stash"), None);
    }

    #[test]
    fn git_subcommand_after_returns_action_when_before_pipe() {
        assert_eq!(
            git_subcommand_after("git stash list | grep wip", "stash").as_deref(),
            Some("list")
        );
    }

    #[test]
    fn git_subcommand_after_unaffected_without_metacharacters() {
        assert_eq!(
            git_subcommand_after("git stash drop", "stash").as_deref(),
            Some("drop")
        );
    }
}
