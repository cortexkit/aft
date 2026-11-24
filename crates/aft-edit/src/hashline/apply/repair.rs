//! Repair layers applied to lowered replacement groups before materialization.
//!
//! Layers owned by this module:
//! - **boundary-echo**: drop payload lines that exactly restate surviving lines
//!   just outside the replaced span.
//! - **indent**: restore a uniformly omitted base indent when unchanged rows
//!   prove the shift.
//! - **replacement-coalescing**: handled in [`super::edits::coalesce_replacement_edits`]
//!   before these layers run.
//!
//! Exact verbatim remap recovery is intentionally out of scope here; that path
//! belongs to the recovery planner and never runs as a silent repair.

use super::edits::{find_replacement_group, InsertMode, InsertPlace, LineEdit, ReplacementGroup};

/// Outcome of running every local repair layer on a lowered edit list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepairOutcome {
    pub edits: Vec<LineEdit>,
    pub warnings: Vec<String>,
    /// Which named repair layers actually rewrote the edit list.
    pub layers_applied: Vec<&'static str>,
}

/// Run indent repair then boundary-echo repair. Coalescing is expected to have
/// already normalized contiguous replacements into groups.
pub fn apply_repair_layers(edits: &[LineEdit], file_lines: &[String]) -> RepairOutcome {
    let mut working = edits.to_vec();
    let mut warnings = Vec::new();
    let mut layers_applied = Vec::new();

    let indent = repair_replacement_indentation(&mut working, file_lines);
    if !indent.is_empty() {
        layers_applied.push("indent");
        warnings.extend(indent);
    }

    let echo = repair_boundary_echoes(&mut working, file_lines);
    if !echo.is_empty() {
        layers_applied.push("boundary-echo");
        warnings.extend(echo);
    }

    RepairOutcome {
        edits: working,
        warnings,
        layers_applied,
    }
}

/// Restore a uniformly omitted base indent only when the payload would escape
/// a surviving `{` opener immediately above the replacement and matching
/// unchanged rows prove the uniform shift.
fn repair_replacement_indentation(edits: &mut [LineEdit], file_lines: &[String]) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut start = 0;
    while start < edits.len() {
        let Some(group) = find_replacement_group(edits, start) else {
            start += 1;
            continue;
        };
        let last = *group.delete_indices.last().unwrap_or(&start);
        start = last + 1;
        if group.payload.len() != group.delete_indices.len() {
            continue;
        }
        let preceding = file_lines
            .get(group.start_line.saturating_sub(2))
            .map(String::as_str)
            .unwrap_or("");
        let source_first = file_lines
            .get(group.start_line.saturating_sub(1))
            .map(String::as_str)
            .unwrap_or("");
        let payload_first = group.payload.first().map(String::as_str).unwrap_or("");
        if !preceding.trim_end().ends_with('{')
            || !is_indent_deeper(leading_indent(source_first), leading_indent(preceding))
            || is_indent_deeper(leading_indent(payload_first), leading_indent(preceding))
        {
            continue;
        }

        let mut shift: Option<String> = None;
        let mut matches = 0usize;
        let mut consistent = true;
        for offset in 0..group.payload.len() {
            let source = file_lines
                .get(group.start_line - 1 + offset)
                .map(String::as_str)
                .unwrap_or("");
            let payload = group.payload[offset].as_str();
            if source.trim().is_empty() || source.trim_start() != payload.trim_start() {
                continue;
            }
            let source_indent = leading_indent(source);
            let payload_indent = leading_indent(payload);
            if !source_indent.ends_with(payload_indent) {
                consistent = false;
                break;
            }
            let candidate = source_indent[..source_indent.len() - payload_indent.len()].to_string();
            match &shift {
                None => shift = Some(candidate),
                Some(existing) if existing != &candidate => {
                    consistent = false;
                    break;
                }
                Some(_) => {}
            }
            matches += 1;
        }
        if !consistent || shift.is_none() || matches < 2 || matches * 2 <= group.payload.len() {
            continue;
        }
        let shift = shift.unwrap();
        for index in &group.insert_indices {
            if let LineEdit::Insert { text, .. } = &mut edits[*index] {
                if !text.trim().is_empty() {
                    *text = format!("{shift}{text}");
                }
            }
        }
        warnings.push(format!(
            "Auto-indented a replacement body at line {}: restored a uniformly omitted base indent.",
            group.start_line
        ));
    }
    warnings
}

/// Drop payload lines that exactly restate surviving lines outside the range.
fn repair_boundary_echoes(edits: &mut Vec<LineEdit>, file_lines: &[String]) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut rebuilt = Vec::with_capacity(edits.len());
    let mut i = 0;
    while i < edits.len() {
        let Some(group) = find_replacement_group(edits, i) else {
            rebuilt.push(edits[i].clone());
            i += 1;
            continue;
        };
        let last = *group.delete_indices.last().unwrap();
        i = last + 1;

        if let Some(echo) = find_boundary_echo(&group, file_lines) {
            let inserts: Vec<LineEdit> = group
                .insert_indices
                .iter()
                .skip(echo.leading)
                .take(group.insert_indices.len() - echo.leading - echo.trailing)
                .map(|idx| edits[*idx].clone())
                .collect();
            let deletes: Vec<LineEdit> = group
                .delete_indices
                .iter()
                .map(|idx| edits[*idx].clone())
                .collect();
            rebuilt.extend(inserts);
            rebuilt.extend(deletes);
            warnings.push(format!(
                "Auto-repaired a replacement boundary echo at line {}: dropped {} leading and {} trailing payload line(s) already present outside the range.",
                group.start_line, echo.leading, echo.trailing
            ));
            continue;
        }

        if let Some((side, count)) = find_one_sided_boundary_echo(&group, file_lines) {
            let inserts: Vec<LineEdit> = match side {
                EchoSide::Leading => group
                    .insert_indices
                    .iter()
                    .skip(count)
                    .map(|idx| edits[*idx].clone())
                    .collect(),
                EchoSide::Trailing => group
                    .insert_indices
                    .iter()
                    .take(group.insert_indices.len() - count)
                    .map(|idx| edits[*idx].clone())
                    .collect(),
            };
            let deletes: Vec<LineEdit> = group
                .delete_indices
                .iter()
                .map(|idx| edits[*idx].clone())
                .collect();
            rebuilt.extend(inserts);
            rebuilt.extend(deletes);
            warnings.push(format!(
                "Auto-repaired a replacement boundary echo at line {}: dropped {} {} payload line(s) identical to the surviving line(s) just outside the range.",
                group.start_line,
                count,
                match side {
                    EchoSide::Leading => "leading",
                    EchoSide::Trailing => "trailing",
                }
            ));
            continue;
        }

        for idx in group
            .insert_indices
            .iter()
            .chain(group.delete_indices.iter())
        {
            rebuilt.push(edits[*idx].clone());
        }
    }
    *edits = rebuilt;
    warnings
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BoundaryEcho {
    leading: usize,
    trailing: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EchoSide {
    Leading,
    Trailing,
}

fn find_boundary_echo(group: &ReplacementGroup, file_lines: &[String]) -> Option<BoundaryEcho> {
    let leading = count_duplicate_leading(group, file_lines);
    if leading == 0 {
        return None;
    }
    let trailing = count_duplicate_trailing(group, file_lines);
    if trailing == 0 {
        return None;
    }
    if leading + trailing >= group.payload.len() {
        return None;
    }
    Some(BoundaryEcho { leading, trailing })
}

fn find_one_sided_boundary_echo(
    group: &ReplacementGroup,
    file_lines: &[String],
) -> Option<(EchoSide, /* count */ usize)> {
    let leading = count_duplicate_leading(group, file_lines);
    let trailing = count_duplicate_trailing(group, file_lines);
    if (leading > 0) == (trailing > 0) {
        return None;
    }
    let (side, count) = if leading > 0 {
        (EchoSide::Leading, leading)
    } else {
        (EchoSide::Trailing, trailing)
    };
    if count >= group.payload.len() {
        return None;
    }
    // Single-line ranges only drop trailing structural closers.
    if group.delete_indices.len() <= 1 {
        if side != EchoSide::Trailing {
            return None;
        }
        let echo_lines = &group.payload[group.payload.len() - count..];
        if !echo_lines.iter().all(|line| is_structural_closer(line)) {
            return None;
        }
    }
    Some((side, count))
}

fn count_duplicate_leading(group: &ReplacementGroup, file_lines: &[String]) -> usize {
    let max = group.payload.len().min(group.start_line.saturating_sub(1));
    for count in (1..=max).rev() {
        let mut matches = true;
        let mut has_content = false;
        for offset in 0..count {
            let line = &group.payload[offset];
            let file_idx = group.start_line - 1 - count + offset;
            if file_lines.get(file_idx).map(String::as_str) != Some(line.as_str()) {
                matches = false;
                break;
            }
            has_content |= has_non_whitespace(line);
        }
        if matches && has_content {
            return count;
        }
    }
    0
}

fn count_duplicate_trailing(group: &ReplacementGroup, file_lines: &[String]) -> usize {
    let max = group
        .payload
        .len()
        .min(file_lines.len().saturating_sub(group.end_line));
    for count in (1..=max).rev() {
        let mut matches = true;
        let mut has_content = false;
        for offset in 0..count {
            let line = &group.payload[group.payload.len() - count + offset];
            let file_idx = group.end_line + offset;
            if file_lines.get(file_idx).map(String::as_str) != Some(line.as_str()) {
                matches = false;
                break;
            }
            has_content |= has_non_whitespace(line);
        }
        if matches && has_content {
            return count;
        }
    }
    0
}

fn leading_indent(line: &str) -> &str {
    let end = line
        .bytes()
        .position(|b| b != b' ' && b != b'\t')
        .unwrap_or(line.len());
    &line[..end]
}

fn is_indent_deeper(deeper: &str, shallower: &str) -> bool {
    deeper.len() > shallower.len() && deeper.starts_with(shallower)
}

fn has_non_whitespace(text: &str) -> bool {
    text.bytes()
        .any(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
}

fn is_structural_closer(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Pure closer lines: `}`, `);`, `});`, `/>`, `</tag>`, `]`, etc.
    let bytes = trimmed.as_bytes();
    let first = bytes[0];
    matches!(first, b'}' | b')' | b']' | b'/')
        || trimmed.starts_with("</")
        || trimmed
            .chars()
            .all(|c| matches!(c, '}' | ')' | ']' | ';' | ',' | '/' | '>'))
}

/// Build a synthetic replacement group for unit tests and negative controls.
pub fn replacement_group_from_payload(
    start_line: usize,
    end_line: usize,
    payload: Vec<String>,
) -> (Vec<LineEdit>, ReplacementGroup) {
    let mut edits = Vec::new();
    for text in &payload {
        edits.push(LineEdit::Insert {
            anchor: start_line,
            place: InsertPlace::Before,
            text: text.clone(),
            mode: InsertMode::Replacement,
            op_index: 0,
        });
    }
    for line in start_line..=end_line {
        edits.push(LineEdit::Delete { line, op_index: 0 });
    }
    let group = find_replacement_group(&edits, 0).expect("constructed group");
    (edits, group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::apply::edits::materialize_edits;

    #[test]
    fn boundary_echo_drops_restated_neighbors() {
        let file = vec![
            "keep-above".into(),
            "old-a".into(),
            "old-b".into(),
            "keep-below".into(),
        ];
        let (mut edits, _) = replacement_group_from_payload(
            2,
            3,
            vec![
                "keep-above".into(),
                "new-a".into(),
                "new-b".into(),
                "keep-below".into(),
            ],
        );
        let warnings = repair_boundary_echoes(&mut edits, &file);
        assert!(!warnings.is_empty());
        let result = materialize_edits(&file, &edits);
        assert_eq!(
            result,
            vec![
                "keep-above".to_string(),
                "new-a".into(),
                "new-b".into(),
                "keep-below".into()
            ]
        );
    }

    #[test]
    fn indent_repair_restores_uniform_base() {
        let file = vec![
            "    if (value > 90) {".into(),
            "      result = error;".into(),
            "    } else if (value > 70) {".into(),
            "      result = plain;".into(),
            "    } else {".into(),
            "      result = warning;".into(),
            "    }".into(),
        ];
        let (mut edits, _) = replacement_group_from_payload(
            2,
            6,
            vec![
                "  result = error;".into(),
                "} else if (value > 70) {".into(),
                "  result = warning;".into(),
                "} else {".into(),
                "  result = plain;".into(),
            ],
        );
        let warnings = repair_replacement_indentation(&mut edits, &file);
        assert!(!warnings.is_empty());
        let result = materialize_edits(&file, &edits);
        assert_eq!(result[1], "      result = error;");
        assert!(result[2].starts_with("    }"));
    }

    #[test]
    fn intentional_indent_only_edit_is_not_repaired() {
        let file = vec!["    first();".into(), "    second();".into()];
        let (mut edits, _) =
            replacement_group_from_payload(1, 2, vec!["first();".into(), "second();".into()]);
        let warnings = repair_replacement_indentation(&mut edits, &file);
        assert!(warnings.is_empty());
        assert_eq!(
            materialize_edits(&file, &edits),
            vec!["first();".to_string(), "second();".into()]
        );
    }
}
