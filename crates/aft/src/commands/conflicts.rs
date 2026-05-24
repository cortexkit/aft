//! Handler for the `git_conflicts` command: discover and parse merge conflict regions.
//!
//! Auto-discovers conflicted files via `git ls-files --unmerged`, parses `<<<<<<<`/`=======`/`>>>>>>>`
//! markers, and returns line-numbered conflict regions with surrounding context — the same format
//! agents see from `read`, but only the conflict areas.

use std::path::Path;
use std::process::Command;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Number of context lines to show before and after each conflict block.
const CONTEXT_LINES: usize = 3;

/// A single parsed conflict region within a file.
struct ConflictRegion {
    /// 1-based line number of the `<<<<<<<` marker.
    start_line: usize,
    /// 1-based line number of the `>>>>>>>` marker.
    end_line: usize,
}

/// Find all conflicted files using `git ls-files --unmerged`.
/// Returns unique file paths relative to the git working directory.
fn discover_conflicted_files(project_root: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(["ls-files", "--unmerged"])
        .current_dir(project_root)
        .output()
        .map_err(|e| format!("failed to run git: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Not a git repo or no git installed
        if stderr.contains("not a git repository") {
            return Err("not a git repository".to_string());
        }
        return Err(format!("git ls-files failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in stdout.lines() {
        // Format: "<mode> <hash> <stage>\t<filename>"
        if let Some(tab_pos) = line.find('\t') {
            let filename = &line[tab_pos + 1..];
            if seen.insert(filename.to_string()) {
                files.push(filename.to_string());
            }
        }
    }

    files.sort();
    Ok(files)
}

/// Parse a file's content and find all conflict regions (marker line numbers).
fn find_conflict_regions(content: &str) -> Vec<ConflictRegion> {
    let mut regions = Vec::new();
    let mut current_start: Option<usize> = None;

    for (idx, line) in content.lines().enumerate() {
        let line_num = idx + 1; // 1-based
        if line.starts_with("<<<<<<<") {
            current_start = Some(line_num);
        } else if line.starts_with(">>>>>>>") {
            if let Some(start) = current_start {
                regions.push(ConflictRegion {
                    start_line: start,
                    end_line: line_num,
                });
                current_start = None;
            }
        }
    }

    regions
}

/// Format conflict regions for a single file with line-numbered content and context.
fn format_file_conflicts(
    file_path: &str,
    content: &str,
    regions: &[ConflictRegion],
    context_lines: usize,
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    let mut out = String::new();

    // File header
    let conflict_word = if regions.len() == 1 {
        "conflict"
    } else {
        "conflicts"
    };
    out.push_str(&format!(
        "── {} [{} {}] ──\n",
        file_path,
        regions.len(),
        conflict_word,
    ));

    let mut last_printed_line = 0usize;
    for (i, region) in regions.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }

        // Calculate context window (clamp to file bounds)
        let calculated_ctx_start = if region.start_line > context_lines {
            region.start_line - context_lines
        } else {
            1
        };
        let ctx_start = std::cmp::max(calculated_ctx_start, last_printed_line + 1);
        let ctx_end = std::cmp::min(region.end_line + context_lines, total_lines);

        if ctx_start > ctx_end {
            continue;
        }

        // Output lines with line numbers (matching `read` format)
        for line_num in ctx_start..=ctx_end {
            let line_content = lines.get(line_num - 1).unwrap_or(&"");
            // Right-align line numbers to match read output
            out.push_str(&format!("{:>4}: {}\n", line_num, line_content));
        }
        last_printed_line = ctx_end;
    }

    out
}

/// Handle a `git_conflicts` request.
///
/// No params required. Auto-discovers conflicted files via git and returns
/// line-numbered conflict regions with context.
///
/// Returns text output with conflict regions formatted like `read` output.
pub fn handle_git_conflicts(ctx: &AppContext, req: &RawRequest) -> Response {
    let project_root = match &ctx.config().project_root {
        Some(root) => std::path::PathBuf::from(root),
        None => std::env::current_dir().unwrap_or_default(),
    };
    let context_lines = req
        .params
        .get("context_lines")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(CONTEXT_LINES);

    // Discover conflicted files
    let files = match discover_conflicted_files(&project_root) {
        Ok(f) => f,
        Err(e) => {
            return Response::error(&req.id, "git_error", e);
        }
    };

    if files.is_empty() {
        return Response::success(
            &req.id,
            serde_json::json!({
                "text": "No merge conflicts found.",
                "file_count": 0,
                "conflict_count": 0,
            }),
        );
    }

    let mut output = String::new();
    let mut total_conflicts = 0;
    let mut files_with_conflicts = 0;

    for file_path in &files {
        let full_path = project_root.join(file_path);
        let validated_path = match ctx.validate_path(&req.id, &full_path) {
            Ok(path) => path,
            Err(resp) => return resp,
        };

        // Read file content
        let content = match std::fs::read_to_string(&validated_path) {
            Ok(c) => c,
            Err(e) => {
                output.push_str(&format!("── {} [error: {}] ──\n\n", file_path, e));
                continue;
            }
        };

        // Find conflict regions
        let regions = find_conflict_regions(&content);
        if regions.is_empty() {
            // File is in unmerged state but has no conflict markers
            // (could be a deleted-vs-modified conflict)
            output.push_str(&format!(
                "── {} [unmerged — no conflict markers found] ──\n\n",
                file_path
            ));
            continue;
        }

        total_conflicts += regions.len();
        files_with_conflicts += 1;

        // Format this file's conflicts
        let formatted = format_file_conflicts(file_path, &content, &regions, context_lines);
        output.push_str(&formatted);
        output.push('\n');
    }

    // Prepend summary header
    let header = format!(
        "{} {}, {} {}\n\n",
        files_with_conflicts,
        if files_with_conflicts == 1 {
            "file"
        } else {
            "files"
        },
        total_conflicts,
        if total_conflicts == 1 {
            "conflict"
        } else {
            "conflicts"
        },
    );

    let text = format!("{}{}", header, output.trim_end());

    Response::success(
        &req.id,
        serde_json::json!({
            "text": text,
            "file_count": files_with_conflicts,
            "conflict_count": total_conflicts,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_conflict_regions_basic() {
        let content = r#"line 1
line 2
<<<<<<< HEAD
our change
=======
their change
>>>>>>> upstream/dev
line 8
"#;
        let regions = find_conflict_regions(content);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start_line, 3);
        assert_eq!(regions[0].end_line, 7);
    }

    #[test]
    fn test_find_conflict_regions_multiple() {
        let content = r#"line 1
<<<<<<< HEAD
ours 1
=======
theirs 1
>>>>>>> dev
line 7
line 8
<<<<<<< HEAD
ours 2
=======
theirs 2
>>>>>>> dev
line 14
"#;
        let regions = find_conflict_regions(content);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].start_line, 2);
        assert_eq!(regions[0].end_line, 6);
        assert_eq!(regions[1].start_line, 9);
        assert_eq!(regions[1].end_line, 13);
    }

    #[test]
    fn test_find_conflict_regions_diff3() {
        let content = r#"before
<<<<<<< HEAD
our code
||||||| base
base code
=======
their code
>>>>>>> upstream
after
"#;
        let regions = find_conflict_regions(content);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start_line, 2);
        assert_eq!(regions[0].end_line, 8);
    }

    #[test]
    fn test_find_conflict_regions_none() {
        let content = "no conflicts here\njust normal code\n";
        let regions = find_conflict_regions(content);
        assert_eq!(regions.len(), 0);
    }

    #[test]
    fn test_format_file_conflicts() {
        let content = r#"line 1
line 2
line 3
<<<<<<< HEAD
our change
=======
their change
>>>>>>> upstream/dev
line 9
line 10
line 11"#;
        let regions = find_conflict_regions(content);
        let output = format_file_conflicts("src/foo.ts", content, &regions, 3);

        assert!(output.contains("── src/foo.ts [1 conflict] ──"));
        assert!(output.contains("   1: line 1"));
        assert!(output.contains("   4: <<<<<<< HEAD"));
        assert!(output.contains("   5: our change"));
        assert!(output.contains("   6: ======="));
        assert!(output.contains("   7: their change"));
        assert!(output.contains("   8: >>>>>>> upstream/dev"));
        assert!(output.contains("  11: line 11"));
    }

    #[test]
    fn test_format_file_conflicts_context_clamp() {
        // Conflict at the very start of file — context shouldn't go negative
        let content = r#"<<<<<<< HEAD
ours
=======
theirs
>>>>>>> dev
line 6"#;
        let regions = find_conflict_regions(content);
        let output = format_file_conflicts("start.ts", content, &regions, 3);

        assert!(output.contains("   1: <<<<<<< HEAD"));
        assert!(output.contains("   6: line 6"));
    }

    #[test]
    fn test_format_plural_conflicts() {
        let content = r#"<<<<<<< HEAD
a
=======
b
>>>>>>> dev
middle
<<<<<<< HEAD
c
=======
d
>>>>>>> dev"#;
        let regions = find_conflict_regions(content);
        let output = format_file_conflicts("multi.ts", content, &regions, 1);

        assert!(output.contains("[2 conflicts]"));
    }
}
