//! Handler for the `read` command: fast file/directory reading with line numbers.
//!
//! This is the simple "give me file contents" command, designed to replace
//! opencode's built-in read tool with a faster Rust implementation.
//! For symbol-based reading and call-graph annotations, use `zoom` instead.

use std::fs;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::Path;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

const DEFAULT_LIMIT: u32 = 2000;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_BYTES: usize = 50 * 1024; // 50KB output cap
const MAX_FILE_READ_BYTES: u64 = 50 * 1024 * 1024; // 50MB input guard
const MAX_DIRECTORY_ENTRIES: usize = 1000;
const BINARY_SAMPLE_BYTES: usize = 4 * 1024;

/// Check if file content is binary using the content_inspector crate.
/// Detects null bytes, UTF-16 BOMs, and other binary indicators.
fn is_binary(content: &[u8]) -> bool {
    content_inspector::inspect(content).is_binary()
}
/// Handle a `read` request.
///
/// Params:
///   - `file` (string, required) — path to file or directory
///   - `start_line` (u32, optional) — 1-based start line (default: 1)
///   - `end_line` (u32, optional) — 1-based end line (default: start_line + limit - 1)
///   - `limit` (u32, optional) — max lines to return (default: 2000)
///
/// Returns for files:
///   `{ content, complete, total_lines, lines_read, start_line, end_line, truncated, byte_size }`
///
/// `complete` is false whenever the returned content is a slice/truncated; in
/// that case `truncated`, `total_lines`, and the returned range describe the gap.
///
/// Returns for directories:
///   `{ entries[], total_entries }`
///
/// Returns for binary files:
///   `{ binary: true, byte_size }`
pub fn handle_read(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "read: missing required param 'file'",
            );
        }
    };

    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    // Check existence
    if !path.exists() {
        return Response::error(
            &req.id,
            "not_found",
            format!("read: file not found: {}", file),
        );
    }

    // Directory listing
    if path.is_dir() {
        return handle_directory(req, path.as_path());
    }

    let metadata = match fs::metadata(path.as_path()) {
        Ok(metadata) => metadata,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to stat file: {}", e),
            );
        }
    };

    // Parse range parameters
    let limit = req
        .params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(DEFAULT_LIMIT);

    let start_line = req
        .params
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|v| v.max(1) as u32)
        .unwrap_or(1);

    let explicit_end_line = req.params.get("end_line").and_then(|v| v.as_u64());
    let has_explicit_range = req.params.get("start_line").is_some() || explicit_end_line.is_some();

    if has_explicit_range {
        return handle_streaming_range_read(
            req,
            path.as_path(),
            metadata.len(),
            start_line,
            explicit_end_line,
            limit,
        );
    }

    if metadata.len() > MAX_FILE_READ_BYTES {
        return Response::error(
            &req.id,
            "invalid_request",
            format!(
                "read: file is too large to load at once ({} bytes > {} bytes). Use start_line/end_line to read sections.",
                metadata.len(),
                MAX_FILE_READ_BYTES
            ),
        );
    }

    // Read raw bytes for binary detection
    let raw_bytes = match fs::read(path.as_path()) {
        Ok(b) => b,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read file: {}", e),
            );
        }
    };

    let byte_size = raw_bytes.len();

    // Binary detection
    if is_binary(&raw_bytes) {
        return Response::success(
            &req.id,
            serde_json::json!({
                "binary": true,
                "complete": true,
                "byte_size": byte_size,
                "message": format!("Binary file ({} bytes), cannot display as text", byte_size),
            }),
        );
    }

    // Convert to string
    let content = match String::from_utf8(raw_bytes) {
        Ok(s) => s,
        Err(_) => {
            return Response::success(
                &req.id,
                serde_json::json!({
                    "binary": true,
                    "complete": true,
                    "byte_size": byte_size,
                    "message": format!("Binary file ({} bytes), not valid UTF-8", byte_size),
                }),
            );
        }
    };

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len() as u32;

    let end_line = req
        .params
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or_else(|| {
            start_line
                .saturating_add(limit)
                .saturating_sub(1)
                .min(total_lines)
        });

    // Clamp to actual line count. `.max(start_idx)` guards against agents
    // sending inverted ranges (e.g. end_line < start_line) which would
    // otherwise panic at `lines[start_idx..end_idx]` below. With this guard,
    // inverted ranges yield an empty slice and return zero lines.
    let start_idx = (start_line.saturating_sub(1) as usize).min(lines.len());
    let end_idx = (end_line as usize).min(lines.len()).max(start_idx);

    if start_idx >= lines.len() {
        return Response::success(
            &req.id,
            serde_json::json!({
                "content": "",
                "complete": true,
                "total_lines": total_lines,
                "lines_read": 0,
                "start_line": start_line,
                "end_line": start_line,
                "truncated": false,
                "byte_size": byte_size,
            }),
        );
    }

    // Build line-numbered output with truncation
    let mut output = String::new();
    let mut output_bytes = 0usize;
    let mut lines_read = 0u32;
    let mut truncated_by_size = false;

    let line_num_width = format!("{}", end_idx).len();

    for (i, line) in lines[start_idx..end_idx].iter().enumerate() {
        let line_num = start_idx + i + 1; // 1-based
        let display_line = if line.len() > MAX_LINE_LENGTH {
            // Find a safe UTF-8 boundary at or before MAX_LINE_LENGTH to avoid
            // panicking on multi-byte characters (e.g. emoji, CJK).
            let safe_end = line.floor_char_boundary(MAX_LINE_LENGTH);
            format!(
                "{:>width$}: {}... (truncated)\n",
                line_num,
                &line[..safe_end],
                width = line_num_width
            )
        } else {
            format!("{:>width$}: {}\n", line_num, line, width = line_num_width)
        };

        output_bytes += display_line.len();
        if output_bytes > MAX_BYTES {
            truncated_by_size = true;
            // Add truncation notice
            output.push_str(&format!(
                "... (output truncated at {}KB, use start_line/end_line to read sections)\n",
                MAX_BYTES / 1024
            ));
            break;
        }

        output.push_str(&display_line);
        lines_read += 1;
    }

    let actual_end = start_line + lines_read - if lines_read > 0 { 1 } else { 0 };
    let has_more = (start_idx > 0) || (end_idx as u32) < total_lines || truncated_by_size;

    Response::success(
        &req.id,
        serde_json::json!({
            "content": output,
            "complete": !has_more,
            "total_lines": total_lines,
            "lines_read": lines_read,
            "start_line": start_line,
            "end_line": actual_end,
            "truncated": has_more,
            "byte_size": byte_size,
        }),
    )
}

fn handle_streaming_range_read(
    req: &RawRequest,
    path: &Path,
    byte_size: u64,
    start_line: u32,
    explicit_end_line: Option<u64>,
    limit: u32,
) -> Response {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read file: {}", e),
            );
        }
    };

    let mut sample = [0u8; BINARY_SAMPLE_BYTES];
    let sample_len = match file.read(&mut sample) {
        Ok(len) => len,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read file: {}", e),
            );
        }
    };

    if is_binary(&sample[..sample_len]) {
        return Response::success(
            &req.id,
            serde_json::json!({
                "binary": true,
                "complete": true,
                "byte_size": byte_size as usize,
                "message": format!("Binary file ({} bytes), cannot display as text", byte_size),
            }),
        );
    }

    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        return Response::error(
            &req.id,
            "io_error",
            format!("read: failed to read file: {}", e),
        );
    }

    let requested_end_line = explicit_end_line
        .map(|v| v as u32)
        .unwrap_or_else(|| start_line.saturating_add(limit).saturating_sub(1));
    let requested_start_idx = start_line.saturating_sub(1) as usize;
    let requested_end_idx = (requested_end_line as usize).max(requested_start_idx);

    let mut selected_lines = Vec::new();
    let mut observed_lines = 0u32;
    let mut invalid_utf8 = false;
    let reader = std::io::BufReader::new(file);

    for (index, line_result) in reader.lines().enumerate() {
        let line = match line_result {
            Ok(line) => line,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                invalid_utf8 = true;
                break;
            }
            Err(e) => {
                return Response::error(
                    &req.id,
                    "io_error",
                    format!("read: failed to read file: {}", e),
                );
            }
        };

        observed_lines = observed_lines.saturating_add(1);
        if index >= requested_start_idx && index < requested_end_idx {
            selected_lines.push(line);
        }
    }

    if invalid_utf8 {
        return Response::success(
            &req.id,
            serde_json::json!({
                "binary": true,
                "complete": true,
                "byte_size": byte_size as usize,
                "message": format!("Binary file ({} bytes), not valid UTF-8", byte_size),
            }),
        );
    }

    if selected_lines.is_empty() {
        return Response::success(
            &req.id,
            serde_json::json!({
                "content": "",
                "complete": true,
                "total_lines": observed_lines,
                "lines_read": 0,
                "start_line": start_line,
                "end_line": start_line,
                "truncated": false,
                "byte_size": byte_size as usize,
            }),
        );
    }

    let mut output = String::new();
    let mut output_bytes = 0usize;
    let mut lines_read = 0u32;
    let mut truncated_by_size = false;
    let line_num_width = format!("{}", requested_start_idx + selected_lines.len()).len();

    for (i, line) in selected_lines.iter().enumerate() {
        let line_num = requested_start_idx + i + 1;
        let display_line = if line.len() > MAX_LINE_LENGTH {
            let safe_end = line.floor_char_boundary(MAX_LINE_LENGTH);
            format!(
                "{:>width$}: {}... (truncated)\n",
                line_num,
                &line[..safe_end],
                width = line_num_width
            )
        } else {
            format!("{:>width$}: {}\n", line_num, line, width = line_num_width)
        };

        output_bytes += display_line.len();
        if output_bytes > MAX_BYTES {
            truncated_by_size = true;
            output.push_str(&format!(
                "... (output truncated at {}KB, use start_line/end_line to read sections)\n",
                MAX_BYTES / 1024
            ));
            break;
        }

        output.push_str(&display_line);
        lines_read += 1;
    }

    let actual_end = start_line + lines_read - if lines_read > 0 { 1 } else { 0 };
    let has_more = requested_start_idx > 0 || (requested_end_idx as u32) < observed_lines;
    let truncated = has_more || truncated_by_size;

    Response::success(
        &req.id,
        serde_json::json!({
            "content": output,
            "complete": !truncated,
            "total_lines": observed_lines,
            "lines_read": lines_read,
            "start_line": start_line,
            "end_line": actual_end,
            "truncated": truncated,
            "byte_size": byte_size as usize,
        }),
    )
}

/// Handle directory listing.
fn handle_directory(req: &RawRequest, path: &Path) -> Response {
    let mut entries: Vec<String> = Vec::new();

    let read_dir = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => {
            return Response::error(
                &req.id,
                "io_error",
                format!("read: failed to read directory: {}", e),
            );
        }
    };

    for entry_result in read_dir {
        let entry = match entry_result {
            Ok(e) => e,
            Err(_) => continue,
        };

        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);

        if is_dir {
            entries.push(format!("{}/", name));
        } else {
            entries.push(name);
        }
    }

    entries.sort();

    let total = entries.len();
    let truncated = total > MAX_DIRECTORY_ENTRIES;
    if truncated {
        entries.truncate(MAX_DIRECTORY_ENTRIES);
        entries.push(format!(
            "\n... and {} more entries (truncated, showing first 1000)",
            total - MAX_DIRECTORY_ENTRIES
        ));
    }
    Response::success(
        &req.id,
        serde_json::json!({
            "entries": entries,
            "complete": !truncated,
            "truncated": truncated,
            "total_entries": total,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_binary_detects_null_bytes() {
        assert!(is_binary(&[0x48, 0x65, 0x6c, 0x00, 0x6f]));
        assert!(!is_binary(b"Hello, world!"));
        assert!(!is_binary(b""));
    }

    #[test]
    fn test_is_binary_checks_first_8kb() {
        let mut data = vec![0x41u8; 16384]; // 16KB of 'A'
        data[10000] = 0; // null byte after 8KB boundary
        assert!(!is_binary(&data)); // should not detect — null is past 8KB

        data[100] = 0; // null byte within 8KB
        assert!(is_binary(&data));
    }
}
