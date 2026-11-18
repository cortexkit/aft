use super::*;

pub(super) fn incomplete_analysis_reason(value: &Value) -> String {
    incomplete_analysis_reason_with_files(value, true)
}

pub(super) fn incomplete_analysis_reason_with_files(value: &Value, include_files: bool) -> String {
    let mut reasons = Vec::new();
    if let Some(files) = value
        .get("parse_errors")
        .and_then(Value::as_array)
        .filter(|files| !files.is_empty())
    {
        let label = if files.len() == 1 { "file" } else { "files" };
        let names = files
            .iter()
            .filter(|_| include_files)
            .take(MAX_PARSE_FAILURE_FILES)
            .filter_map(|file| {
                let path = file.get("file").and_then(Value::as_str)?;
                Some(
                    match file
                        .get("message")
                        .or_else(|| file.get("reason"))
                        .and_then(Value::as_str)
                    {
                        Some(reason) => format!(
                            "{path} ({})",
                            reason.split_whitespace().collect::<Vec<_>>().join(" ")
                        ),
                        None => path.to_string(),
                    },
                )
            })
            .collect::<Vec<_>>();
        let mut reason = format!("{} {label} could not be parsed", files.len());
        if !names.is_empty() {
            reason.push_str(": ");
            reason.push_str(&names.join(", "));
            let remaining = files.len().saturating_sub(names.len());
            if remaining > 0 {
                reason.push_str(&format!(", +{remaining} more"));
            }
        }
        reasons.push(reason);
    }
    if let Some(files) = value
        .get("skipped_files")
        .and_then(Value::as_array)
        .filter(|files| !files.is_empty())
    {
        // Complexity and duplicates cap their list and carry the full total
        // in `skipped_files_count`; their line says how many files were
        // skipped and why. Other categories keep their own wording.
        let total = value.get("skipped_files_count").and_then(Value::as_u64);
        let count = total.unwrap_or(files.len() as u64);
        let label = if count == 1 { "file" } else { "files" };
        let mut causes = files
            .iter()
            .map(|file| file.get("reason").and_then(Value::as_str));
        let first = causes.next().flatten();
        let shared_cause = first.filter(|first| causes.all(|cause| cause == Some(*first)));
        match (total, shared_cause) {
            (Some(_), Some(cause)) => reasons.push(format!("{count} {label} skipped ({cause})")),
            (Some(_), None) => reasons.push(format!("{count} {label} skipped")),
            (None, _) => reasons.push(format!("{count} {label} could not be analyzed")),
        }
    }
    if !reasons.is_empty() {
        return reasons.join("; ");
    }
    compact_inspect_reason(
        value
            .get("reason")
            .or_else(|| value.get("callgraph_unavailable_reason"))
            .and_then(Value::as_str)
            .unwrap_or("analysis did not finish"),
    )
}

pub(super) fn attach_complexity_size_skips(
    summary: &Map<String, Value>,
    details: &mut Map<String, Value>,
    project_root: &Path,
    top_k: usize,
    offset: usize,
) {
    use crate::inspect::scanners::source_text::{
        bounded_line_count, SKIPPED_FILES_LIMIT, TOO_LARGE,
    };
    let Some(section) = summary.get("complexity") else {
        return;
    };
    let Some(files) = section.get("skipped_files").and_then(Value::as_array) else {
        return;
    };
    // Apply the display cap before opening files. Line counts are only useful
    // for the paths the agent will actually see, not the hidden remainder.
    let rows = files
        .iter()
        .skip(offset)
        .take(top_k.min(SKIPPED_FILES_LIMIT))
        .map(|file| {
            let mut file = file.clone();
            if file["reason"] == TOO_LARGE {
                if let Some(path) = file["file"].as_str() {
                    if let Ok((lines, exact)) = bounded_line_count(&project_root.join(path)) {
                        file["lines"] = serde_json::json!(lines);
                        file["lines_exact"] = serde_json::json!(exact);
                    }
                }
            }
            file
        })
        .collect::<Vec<_>>();
    let shown = rows.len();
    let total = section
        .get("skipped_files_count")
        .and_then(Value::as_u64)
        .unwrap_or(files.len() as u64) as usize;
    details.insert("complexity_skipped_files".to_string(), Value::Array(rows));
    crate::list_surfaces::inspect::attach_inspect_page_envelope(
        details,
        "complexity_skipped_files",
        shown,
        total,
        offset,
    );
}

pub(super) fn render_complexity_skipped_files(
    lines: &mut Vec<String>,
    details: &Map<String, Value>,
) {
    let Some(files) = details
        .get("complexity_skipped_files")
        .and_then(Value::as_array)
    else {
        return;
    };
    lines.push("Complexity skipped files:".to_string());
    for file in files {
        let path = file["file"].as_str().unwrap_or("?");
        let reason = file["reason"].as_str().unwrap_or("unavailable");
        let count = file["lines"]
            .as_u64()
            .map(|count| {
                let qualifier = if file["lines_exact"] == false {
                    "at least "
                } else {
                    ""
                };
                format!("{qualifier}{count} lines; ")
            })
            .unwrap_or_else(|| {
                if reason == crate::inspect::scanners::source_text::TOO_LARGE {
                    "line count unavailable; ".to_string()
                } else {
                    String::new()
                }
            });
        lines.push(format!("  {path} ({count}{reason})"));
    }
    if let Some(trailer) =
        crate::list_surfaces::inspect::trailer_from_details(details, "complexity_skipped_files")
    {
        lines.push(trailer);
    }
}
