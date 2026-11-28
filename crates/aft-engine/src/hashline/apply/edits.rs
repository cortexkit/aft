//! Low-level line edits produced by lowering PUT/CUT operations.
//!
//! Replacement payloads are tagged so repair layers can distinguish a true
//! replacement group (inserts + matching deletes) from ordinary gap inserts.

use crate::hashline::scan::TerminatorKind;

/// One concrete edit against pre-request line coordinates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LineEdit {
    Insert {
        /// 1-based anchor line. For BOF inserts this is 1 with [`InsertPlace::Before`].
        anchor: usize,
        place: InsertPlace,
        text: String,
        mode: InsertMode,
        /// Source operation index inside the section, used to group replacements.
        op_index: usize,
    },
    Delete {
        line: usize,
        op_index: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InsertPlace {
    Before,
    After,
    /// Insert before the first line of the file, even when the file is empty.
    Bof,
    /// Append after the last retained line.
    Eof,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InsertMode {
    Plain,
    Replacement,
}

/// A replacement group: contiguous replacement inserts sharing one op, followed
/// by contiguous deletes for the same op. Mirrors the oracle's lowered form of
/// `PUT N-M:` with a body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplacementGroup {
    pub insert_indices: Vec<usize>,
    pub delete_indices: Vec<usize>,
    pub payload: Vec<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub op_index: usize,
}

/// Detect a replacement group starting at `start` in the edit list.
pub fn find_replacement_group(edits: &[LineEdit], start: usize) -> Option<ReplacementGroup> {
    let LineEdit::Insert {
        anchor,
        place: InsertPlace::Before,
        mode: InsertMode::Replacement,
        op_index,
        ..
    } = edits.get(start)?
    else {
        return None;
    };
    let anchor_line = *anchor;
    let op_index = *op_index;
    let mut insert_indices = Vec::new();
    let mut payload = Vec::new();
    let mut i = start;
    while i < edits.len() {
        match &edits[i] {
            LineEdit::Insert {
                anchor,
                place: InsertPlace::Before,
                text,
                mode: InsertMode::Replacement,
                op_index: edit_op,
            } if *anchor == anchor_line && *edit_op == op_index => {
                insert_indices.push(i);
                payload.push(text.clone());
                i += 1;
            }
            _ => break,
        }
    }
    let mut delete_indices = Vec::new();
    let mut expected = anchor_line;
    while i < edits.len() {
        match &edits[i] {
            LineEdit::Delete {
                line,
                op_index: edit_op,
            } if *line == expected && *edit_op == op_index => {
                delete_indices.push(i);
                expected += 1;
                i += 1;
            }
            _ => break,
        }
    }
    if delete_indices.is_empty() {
        return None;
    }
    Some(ReplacementGroup {
        insert_indices,
        delete_indices,
        payload,
        start_line: anchor_line,
        end_line: expected - 1,
        op_index,
    })
}

/// Coalesce adjacent or overlapping replacement groups that share the same
/// source operation into one contiguous replacement. This is the
/// replacement-coalescing repair layer: agents sometimes emit several
/// single-line PUTs that together replace a contiguous span.
pub fn coalesce_replacement_edits(edits: &[LineEdit]) -> Vec<LineEdit> {
    if edits.is_empty() {
        return Vec::new();
    }

    // Group by op_index while preserving first-seen order.
    let mut op_order: Vec<usize> = Vec::new();
    let mut by_op: std::collections::BTreeMap<usize, Vec<LineEdit>> =
        std::collections::BTreeMap::new();
    for edit in edits {
        let op = match edit {
            LineEdit::Insert { op_index, .. } | LineEdit::Delete { op_index, .. } => *op_index,
        };
        if !by_op.contains_key(&op) {
            op_order.push(op);
        }
        by_op.entry(op).or_default().push(edit.clone());
    }

    let mut out = Vec::with_capacity(edits.len());
    for op in op_order {
        let group = by_op.remove(&op).unwrap_or_default();
        out.extend(coalesce_one_op(group));
    }
    out
}

fn coalesce_one_op(edits: Vec<LineEdit>) -> Vec<LineEdit> {
    let mut deletes: Vec<usize> = edits
        .iter()
        .filter_map(|edit| match edit {
            LineEdit::Delete { line, .. } => Some(*line),
            _ => None,
        })
        .collect();
    let replacements: Vec<String> = edits
        .iter()
        .filter_map(|edit| match edit {
            LineEdit::Insert {
                text,
                mode: InsertMode::Replacement,
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect();
    let plain: Vec<LineEdit> = edits
        .iter()
        .filter(|edit| {
            !matches!(
                edit,
                LineEdit::Delete { .. }
                    | LineEdit::Insert {
                        mode: InsertMode::Replacement,
                        ..
                    }
            )
        })
        .cloned()
        .collect();

    if deletes.is_empty() || replacements.is_empty() {
        return edits;
    }

    deletes.sort_unstable();
    deletes.dedup();
    // Only coalesce when deletes form one contiguous span.
    let contiguous = deletes
        .windows(2)
        .all(|pair| pair[1] == pair[0].saturating_add(1));
    if !contiguous {
        return edits;
    }
    let start = deletes[0];
    let end = *deletes.last().unwrap();
    let op_index = match edits.first() {
        Some(LineEdit::Insert { op_index, .. } | LineEdit::Delete { op_index, .. }) => *op_index,
        None => 0,
    };

    let mut coalesced = plain;
    for text in replacements {
        coalesced.push(LineEdit::Insert {
            anchor: start,
            place: InsertPlace::Before,
            text,
            mode: InsertMode::Replacement,
            op_index,
        });
    }
    for line in start..=end {
        coalesced.push(LineEdit::Delete { line, op_index });
    }
    coalesced
}

/// Materialize edits in baseline order without shifting the retained suffix
/// once for every deleted or replaced line.
pub fn materialize_edits(original_lines: &[String], edits: &[LineEdit]) -> Vec<String> {
    let mut by_line: std::collections::BTreeMap<usize, Vec<&LineEdit>> =
        std::collections::BTreeMap::new();
    let mut bof = Vec::new();
    let mut eof = Vec::new();
    for edit in edits {
        match edit {
            LineEdit::Insert {
                place: InsertPlace::Bof,
                text,
                ..
            } => bof.push(text.clone()),
            LineEdit::Insert {
                place: InsertPlace::Eof,
                text,
                ..
            } => eof.push(text.clone()),
            LineEdit::Insert { anchor, .. } => by_line.entry(*anchor).or_default().push(edit),
            LineEdit::Delete { line, .. } => by_line.entry(*line).or_default().push(edit),
        }
    }
    let mut out = Vec::with_capacity(original_lines.len());
    for (index, current) in original_lines.iter().enumerate() {
        if let Some(bucket) = by_line.get(&(index + 1)) {
            append_line_bucket(&mut out, Some(current), bucket);
        } else {
            out.push(current.clone());
        }
    }
    // The old splice implementation also accepted an anchor just past EOF,
    // while ignoring anchors beyond it.
    if let Some(bucket) = by_line.get(&(original_lines.len() + 1)) {
        append_line_bucket(&mut out, None, bucket);
    }
    // Zero saturates to index zero and was applied after line one. Preserve
    // that edge case against the already transformed first row, not baseline.
    if let Some(bucket) = by_line.get(&0) {
        let mut rows = std::mem::take(&mut out).into_iter();
        let first = rows.next();
        append_line_bucket(&mut out, first.as_ref(), bucket);
        out.extend(rows);
    }
    bof.extend(out);
    bof.extend(eof);
    bof
}

fn append_line_bucket(out: &mut Vec<String>, current: Option<&String>, bucket: &[&LineEdit]) {
    let deleted = bucket
        .iter()
        .any(|edit| matches!(edit, LineEdit::Delete { .. }));
    // Plain-before rows precede replacement rows regardless of their order in
    // the lowered operation list; after rows follow the retained baseline row.
    for replacement in [false, true] {
        for edit in bucket {
            if let LineEdit::Insert {
                place: InsertPlace::Before,
                text,
                mode,
                ..
            } = edit
            {
                if (*mode == InsertMode::Replacement) == replacement {
                    out.push(text.clone());
                }
            }
        }
    }
    if !deleted {
        if let Some(current) = current {
            out.push(current.clone());
        }
    }
    for edit in bucket {
        if let LineEdit::Insert {
            place: InsertPlace::After,
            text,
            ..
        } = edit
        {
            out.push(text.clone());
        }
    }
}

/// Frozen splice oracle, including permissive zero/out-of-range anchors.
#[cfg(test)]
fn materialize_edits_legacy(original_lines: &[String], edits: &[LineEdit]) -> Vec<String> {
    let mut file_lines = original_lines.to_vec();
    let mut bof: Vec<String> = Vec::new();
    let mut eof: Vec<String> = Vec::new();

    // Bucket anchor-targeted edits by line.
    let mut by_line: std::collections::BTreeMap<usize, Vec<&LineEdit>> =
        std::collections::BTreeMap::new();
    for edit in edits {
        match edit {
            LineEdit::Insert {
                place: InsertPlace::Bof,
                text,
                ..
            } => bof.push(text.clone()),
            LineEdit::Insert {
                place: InsertPlace::Eof,
                text,
                ..
            } => eof.push(text.clone()),
            LineEdit::Insert { anchor, .. } => {
                by_line.entry(*anchor).or_default().push(edit);
            }
            LineEdit::Delete { line, .. } => {
                by_line.entry(*line).or_default().push(edit);
            }
        }
    }

    // Apply bottom-up so earlier indices stay valid.
    let lines: Vec<usize> = by_line.keys().copied().collect();
    for line in lines.into_iter().rev() {
        let Some(bucket) = by_line.get(&line) else {
            continue;
        };
        let idx = line.saturating_sub(1);
        if idx > file_lines.len() {
            continue;
        }
        let current = file_lines.get(idx).cloned().unwrap_or_default();
        let mut before = Vec::new();
        let mut after = Vec::new();
        let mut replacement = Vec::new();
        let mut delete_line = false;
        for edit in bucket {
            match edit {
                LineEdit::Insert {
                    place: InsertPlace::Before,
                    text,
                    mode: InsertMode::Replacement,
                    ..
                } => replacement.push(text.clone()),
                LineEdit::Insert {
                    place: InsertPlace::Before,
                    text,
                    ..
                } => before.push(text.clone()),
                LineEdit::Insert {
                    place: InsertPlace::After,
                    text,
                    ..
                } => after.push(text.clone()),
                LineEdit::Delete { .. } => delete_line = true,
                LineEdit::Insert {
                    place: InsertPlace::Bof | InsertPlace::Eof,
                    ..
                } => {}
            }
        }
        if before.is_empty() && replacement.is_empty() && after.is_empty() && !delete_line {
            continue;
        }
        let spliced = if delete_line {
            let mut rows = before;
            rows.extend(replacement);
            rows.extend(after);
            rows
        } else {
            let mut rows = before;
            rows.extend(replacement);
            if idx < file_lines.len() {
                rows.push(current);
            }
            rows.extend(after);
            rows
        };
        if idx < file_lines.len() {
            #[cfg(test)]
            MATERIALIZE_SHIFTED_ROWS
                .with(|count| count.set(count.get() + file_lines.len() - idx - 1));
            file_lines.splice(idx..=idx, spliced);
        } else {
            file_lines.extend(spliced);
        }
    }

    if !bof.is_empty() {
        let mut rows = bof;
        rows.append(&mut file_lines);
        file_lines = rows;
    }
    file_lines.extend(eof);
    file_lines
}

#[cfg(test)]
thread_local! {
    static MATERIALIZE_SHIFTED_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Rebuild file bytes from logical lines using the baseline terminator policy.
pub fn join_lines(
    lines: &[String],
    default_terminator: TerminatorKind,
    trailing_terminator: bool,
) -> Vec<u8> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        out.extend_from_slice(line.as_bytes());
        let is_last = index + 1 == lines.len();
        let term = if is_last && !trailing_terminator {
            TerminatorKind::None
        } else if default_terminator == TerminatorKind::None {
            TerminatorKind::Lf
        } else {
            default_terminator
        };
        match term {
            TerminatorKind::Lf => out.push(b'\n'),
            TerminatorKind::CrLf => out.extend_from_slice(b"\r\n"),
            TerminatorKind::None => {}
        }
    }
    out
}

/// Infer terminator policy from baseline raw records.
pub fn terminator_policy(
    records: &std::collections::BTreeMap<usize, crate::hashline::scan::RawLineRecord>,
) -> (TerminatorKind, bool) {
    let mut lf = 0usize;
    let mut crlf = 0usize;
    let mut trailing = false;
    let last = records.keys().next_back().copied();
    for (&line, record) in records {
        match record.terminator {
            TerminatorKind::Lf => lf += 1,
            TerminatorKind::CrLf => crlf += 1,
            TerminatorKind::None => {}
        }
        if Some(line) == last {
            trailing = record.terminator != TerminatorKind::None;
        }
    }
    let default = if crlf > lf {
        TerminatorKind::CrLf
    } else if lf > 0 || crlf > 0 {
        TerminatorKind::Lf
    } else {
        TerminatorKind::Lf
    };
    (default, trailing || records.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialize_large_cut_does_not_shift_retained_rows() {
        let original = (0..20_000)
            .map(|i| format!("line {i} — payload"))
            .collect::<Vec<_>>();
        let edits = (5_001..=15_000)
            .map(|line| LineEdit::Delete { line, op_index: 0 })
            .collect::<Vec<_>>();
        MATERIALIZE_SHIFTED_ROWS.with(|count| count.set(0));
        let actual = materialize_edits(&original, &edits);
        let expected = original[..5_000]
            .iter()
            .chain(&original[15_000..])
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            join_lines(&actual, TerminatorKind::CrLf, true),
            join_lines(&expected, TerminatorKind::CrLf, true)
        );
        assert_eq!(
            MATERIALIZE_SHIFTED_ROWS.with(|count| count.get()),
            0,
            "rows shifted by per-line splice"
        );
    }

    #[test]
    fn materialize_matches_splice_oracle_for_mixed_and_boundary_edits() {
        let mut seed = 41u64;
        for n in 0..=4 {
            let original = (0..n).map(|i| format!("row {i} — 🌍")).collect::<Vec<_>>();
            for _ in 0..512 {
                let mut edits = Vec::new();
                for op_index in 0..8 {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let anchor = ((seed >> 32) % 8) as usize;
                    let place = match (seed >> 16) % 4 {
                        0 => InsertPlace::Bof,
                        1 => InsertPlace::Eof,
                        2 => InsertPlace::Before,
                        _ => InsertPlace::After,
                    };
                    if seed % 3 == 0 {
                        edits.push(LineEdit::Delete {
                            line: anchor,
                            op_index,
                        });
                    } else {
                        edits.push(LineEdit::Insert {
                            anchor,
                            place,
                            text: format!("new {op_index}"),
                            mode: if seed % 2 == 0 {
                                InsertMode::Plain
                            } else {
                                InsertMode::Replacement
                            },
                            op_index,
                        });
                    }
                }
                assert_eq!(
                    materialize_edits(&original, &edits),
                    materialize_edits_legacy(&original, &edits),
                    "baseline={original:?}, edits={edits:?}"
                );
            }
        }
    }

    #[test]
    fn materialize_replaces_a_span() {
        let original = vec!["a".into(), "b".into(), "c".into()];
        let edits = vec![
            LineEdit::Insert {
                anchor: 2,
                place: InsertPlace::Before,
                text: "B".into(),
                mode: InsertMode::Replacement,
                op_index: 0,
            },
            LineEdit::Delete {
                line: 2,
                op_index: 0,
            },
        ];
        assert_eq!(
            materialize_edits(&original, &edits),
            vec!["a".to_string(), "B".into(), "c".into()]
        );
    }

    #[test]
    fn coalesce_merges_contiguous_single_line_replacements() {
        let edits = vec![
            LineEdit::Insert {
                anchor: 1,
                place: InsertPlace::Before,
                text: "A".into(),
                mode: InsertMode::Replacement,
                op_index: 0,
            },
            LineEdit::Delete {
                line: 1,
                op_index: 0,
            },
            LineEdit::Insert {
                anchor: 2,
                place: InsertPlace::Before,
                text: "B".into(),
                mode: InsertMode::Replacement,
                op_index: 0,
            },
            LineEdit::Delete {
                line: 2,
                op_index: 0,
            },
            LineEdit::Insert {
                anchor: 3,
                place: InsertPlace::Before,
                text: "C".into(),
                mode: InsertMode::Replacement,
                op_index: 0,
            },
            LineEdit::Delete {
                line: 3,
                op_index: 0,
            },
        ];
        let coalesced = coalesce_replacement_edits(&edits);
        let group = find_replacement_group(&coalesced, 0).expect("one group");
        assert_eq!(group.start_line, 1);
        assert_eq!(group.end_line, 3);
        assert_eq!(group.payload, vec!["A", "B", "C"]);
    }
}
