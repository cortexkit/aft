//! Handler for the `git_conflicts` command: discover and parse merge conflict regions.
//!
//! Auto-discovers conflicted files via `git ls-files --unmerged`, parses `<<<<<<<`/`=======`/`>>>>>>>`
//! markers, and returns line-numbered conflict regions with surrounding context — the same format
//! agents see from `read`, but only the conflict areas.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

/// Resolve the git toplevel containing `base_path`, which may be a file or directory.
fn git_toplevel(base_path: &Path) -> Result<PathBuf, String> {
    let working_dir = if base_path.is_file() {
        base_path.parent().unwrap_or(base_path)
    } else {
        base_path
    };
    let output = crate::effective_path::new_command("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(working_dir)
        .output()
        .map_err(|e| format!("failed to run git: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not a git repository") {
            return Err("not a git repository".to_string());
        }
        return Err(format!("git rev-parse failed: {}", stderr.trim()));
    }

    let toplevel = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if toplevel.is_empty() {
        return Err("git rev-parse returned an empty toplevel".to_string());
    }

    Ok(PathBuf::from(toplevel))
}

/// Construct a filesystem path from Git's raw NUL-delimited pathname bytes.
///
/// Git's normal line-oriented output C-quotes names containing non-ASCII or
/// control characters, so that representation must never be used to open files.
fn path_from_git_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
    }

    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Find all conflicted files using index-unmerged state plus tracked working-tree markers.
/// Returns the Git toplevel, unique file paths relative to that toplevel, the
/// subset reported by Git as index-unmerged paths, and an error when scanning
/// tracked files for additional conflict markers was incomplete.
fn discover_conflicted_files(
    base_dir: &Path,
) -> Result<(PathBuf, Vec<PathBuf>, HashSet<PathBuf>, Option<String>), String> {
    let toplevel = git_toplevel(base_dir)?;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    let mut unmerged_files = HashSet::new();

    let output = crate::effective_path::new_command("git")
        .args(["ls-files", "--unmerged", "-z"])
        .current_dir(&toplevel)
        .output()
        .map_err(|e| format!("failed to run git: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not a git repository") {
            return Err("not a git repository".to_string());
        }
        return Err(format!("git ls-files failed: {}", stderr.trim()));
    }

    for record in output.stdout.split(|byte| *byte == b'\0') {
        // Format: "<mode> <hash> <stage>\t<filename>\0"
        if let Some(tab_pos) = record.iter().position(|byte| *byte == b'\t') {
            let path = path_from_git_bytes(&record[tab_pos + 1..]);
            unmerged_files.insert(path.clone());
            if seen.insert(path.clone()) {
                files.push(path);
            }
        }
    }

    let mut sweep_error = None;
    match crate::effective_path::new_command("git")
        .args(["grep", "-z", "-lE", r"^(<<<<<<< |>>>>>>> )"])
        .current_dir(&toplevel)
        .output()
    {
        Ok(grep_output) => {
            let status = grep_output.status.code();
            if status == Some(0) {
                for filename in grep_output.stdout.split(|byte| *byte == b'\0') {
                    if filename.is_empty() {
                        continue;
                    }
                    let path = path_from_git_bytes(filename);
                    if seen.insert(path.clone()) {
                        files.push(path);
                    }
                }
            }

            let stderr = String::from_utf8_lossy(&grep_output.stderr);
            if !stderr.trim().is_empty() {
                sweep_error = Some(format!("git grep marker sweep: {}", stderr.trim()));
            } else if !matches!(status, Some(0) | Some(1)) {
                let status = status
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "terminated by signal".to_string());
                sweep_error = Some(format!("git grep marker sweep failed with status {status}"));
            }
        }
        Err(error) => {
            sweep_error = Some(format!("failed to run git grep marker sweep: {error}"));
        }
    }

    files.sort();
    Ok((toplevel, files, unmerged_files, sweep_error))
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
    let requested_path = req
        .params
        .get("path")
        .or_else(|| {
            req.params
                .get("params")
                .and_then(|params| params.get("path"))
        })
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let discovery_base = if let Some(path) = requested_path {
        let path = PathBuf::from(path);
        let path = if path.is_relative() {
            project_root.join(path)
        } else {
            path
        };
        match ctx.validate_path(&req.id, &path) {
            Ok(path) => path,
            Err(resp) => return resp,
        }
    } else {
        project_root
    };

    // Discover conflicted files
    let (git_toplevel, files, unmerged_files, sweep_error) =
        match discover_conflicted_files(&discovery_base) {
            Ok(result) => result,
            Err(e) => {
                if requested_path.is_some() && e == "not a git repository" {
                    return Response::error(
                        &req.id,
                        "not_a_git_repository",
                        format!(
                            "path is not inside a git repository: {}",
                            discovery_base.display()
                        ),
                    );
                }
                return Response::error(&req.id, "git_error", e);
            }
        };
    let checked_root = git_toplevel.to_string_lossy().to_string();

    if files.is_empty() {
        let text = if sweep_error.is_some() {
            format!(
                "No merge conflicts found, but the marker sweep was incomplete — see sweep_error.\nChecked repo root: {}",
                checked_root
            )
        } else {
            format!(
                "No merge conflicts found.\nChecked repo root: {}",
                checked_root
            )
        };
        let mut data = serde_json::json!({
            "text": text,
            "file_count": 0,
            "conflict_count": 0,
            "unmerged_count": unmerged_files.len(),
            "checked_root": checked_root,
        });
        if let Some(sweep_error) = sweep_error {
            if let Some(data) = data.as_object_mut() {
                data.insert("complete".to_string(), serde_json::json!(false));
                data.insert("sweep_error".to_string(), serde_json::json!(sweep_error));
            }
        }
        return Response::success(&req.id, data);
    }

    let mut output = String::new();
    let mut total_conflicts = 0;
    let mut files_with_conflicts = 0;
    let mut skipped_files = Vec::new();

    for file_path in &files {
        let full_path = git_toplevel.join(file_path);
        let validated_path = match ctx.validate_path(&req.id, &full_path) {
            Ok(path) => path,
            Err(resp) => return resp,
        };
        let display_path = file_path.display().to_string();

        // Read file content
        let content = match std::fs::read_to_string(&validated_path) {
            Ok(c) => c,
            Err(e) => {
                // Keep the existing diagnostic in the text while also exposing
                // index-unmerged read failures as a structured partial result.
                output.push_str(&format!("── {} [error: {}] ──\n\n", display_path, e));
                if unmerged_files.contains(file_path) {
                    skipped_files.push(serde_json::json!({
                        "file": display_path,
                        "reason": format!("read_error: {}", e),
                    }));
                }
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
                display_path
            ));
            continue;
        }

        total_conflicts += regions.len();
        files_with_conflicts += 1;

        // Use the raw path before rendering it because rendering can lossy-convert pathname bytes.
        let formatted = format_file_conflicts(&display_path, &content, &regions, context_lines);
        output.push_str(&formatted);
        output.push('\n');
    }

    // Put incomplete-scan warnings in the first line so readers and automated tools
    // do not mistake a partial conflict scan for a complete result.
    let mut gap_summaries = Vec::new();
    if !skipped_files.is_empty() {
        let skipped_count = skipped_files.len();
        gap_summaries.push(format!(
            "{} unmerged file{} unreadable — see skipped_files",
            skipped_count,
            if skipped_count == 1 { "" } else { "s" },
        ));
    }
    if sweep_error.is_some() {
        gap_summaries.push("marker sweep incomplete — see sweep_error".to_string());
    }
    let gap_summary = if gap_summaries.is_empty() {
        String::new()
    } else {
        format!(" ({})", gap_summaries.join("; "))
    };
    let header = format!(
        "{} {}, {} {}{}\nChecked repo root: {}\n\n",
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
        gap_summary,
        checked_root,
    );

    let text = format!("{}{}", header, output.trim_end());

    let mut data = serde_json::json!({
        "text": text,
        "file_count": files_with_conflicts,
        "conflict_count": total_conflicts,
        "unmerged_count": unmerged_files.len(),
        "checked_root": checked_root,
    });
    if let Some(data) = data.as_object_mut() {
        if !skipped_files.is_empty() {
            data.insert("complete".to_string(), serde_json::json!(false));
            data.insert(
                "skipped_files".to_string(),
                serde_json::json!(skipped_files),
            );
        }
        if let Some(sweep_error) = sweep_error {
            data.insert("complete".to_string(), serde_json::json!(false));
            data.insert("sweep_error".to_string(), serde_json::json!(sweep_error));
        }
    }

    Response::success(&req.id, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::config::Config;
    use crate::context::default_language_provider_factory;
    use crate::protocol::RawRequest;

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
    fn conflicts_path_accepts_file_inside_repo() {
        let temp = tempfile::tempdir().expect("create temp repo");
        let repo = std::fs::canonicalize(temp.path()).expect("canonicalize temp repo");
        let file = repo.join("conflicted.txt");
        std::fs::write(
            &file,
            "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n",
        )
        .expect("write conflicted file");

        let init = crate::effective_path::new_command("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .expect("run git init");
        assert!(init.success(), "git init failed: {init}");
        let add = crate::effective_path::new_command("git")
            .args(["add", "conflicted.txt"])
            .current_dir(&repo)
            .status()
            .expect("run git add");
        assert!(add.success(), "git add failed: {add}");

        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(repo),
                ..Default::default()
            },
        );
        let req = RawRequest {
            id: "conflicts-file-path".to_string(),
            command: "git_conflicts".to_string(),
            lsp_hints: None,
            session_id: None,
            params: json!({ "path": file }),
        };

        let response = handle_git_conflicts(&ctx, &req);

        assert!(
            response.success,
            "a file path inside the repo should select that repo: {:?}",
            response.data
        );
        assert_eq!(response.data["file_count"], 1);
        assert_eq!(response.data["conflict_count"], 1);
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
