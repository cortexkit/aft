//! Affected-region construction for hashline edit responses.
//!
//! After a successful in-memory apply, the engine records which output rows
//! changed. The snapshot publisher then expands each range with its nearest
//! surviving predecessor and successor so chained edits have stable context.

use crate::hashline::snapshot::{AffectedRegion, LineRange};

/// Describe how one lowered operation changed the output line map.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegionDelta {
    /// Replacement or pure insertion that produced `count` output rows starting
    /// at 1-based `start`.
    OutputSpan { start: usize, count: usize },
    /// Pure deletion whose nearest surviving neighbors should be retained.
    /// `at` is the 1-based output line where the deletion landed (the first
    /// surviving line after the hole, or one past the end).
    Deletion { at: usize },
}

/// Build an affected region from ordered per-operation deltas.
///
/// Ranges are coalesced so overlapping or adjacent edits render once. Empty
/// files and pure whole-file removals yield an empty region; the publisher
/// still carries empty-file boundary evidence from the final snapshot.
pub fn build_affected_region(deltas: impl IntoIterator<Item = RegionDelta>) -> AffectedRegion {
    let mut ranges = Vec::new();
    for delta in deltas {
        match delta {
            RegionDelta::OutputSpan { start, count } if count > 0 && start > 0 => {
                ranges.push(LineRange::new(start, start + count - 1));
            }
            RegionDelta::Deletion { at } if at > 0 => {
                // Pure deletion retains neighbors via the publisher's expansion
                // of an empty-at-hole marker. Recording `at` as a zero-width
                // seed is represented by a single-line range at the hole so
                // predecessor/successor selection still runs.
                ranges.push(LineRange::new(at, at));
            }
            _ => {}
        }
    }
    AffectedRegion::new(ranges)
}

/// Compute output-line deltas for a single-file apply by comparing original and
/// final logical lines. This is the deterministic fallback used when the
/// operation list does not carry explicit landing metadata.
pub fn affected_from_line_diff(before: &[String], after: &[String]) -> AffectedRegion {
    if before.is_empty() && after.is_empty() {
        return AffectedRegion::default();
    }
    if before.is_empty() {
        return AffectedRegion::insertion(1, after.len());
    }
    if after.is_empty() {
        // Deletion to empty file: zero rows; publisher keeps empty-file evidence.
        return AffectedRegion::default();
    }

    // Longest common prefix/suffix, then the middle is the changed span.
    let mut prefix = 0usize;
    while prefix < before.len() && prefix < after.len() && before[prefix] == after[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < before.len().saturating_sub(prefix)
        && suffix < after.len().saturating_sub(prefix)
        && before[before.len() - 1 - suffix] == after[after.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let after_mid = after.len().saturating_sub(prefix + suffix);
    if after_mid == 0 {
        // Pure deletion in the middle: seed at the first surviving line after
        // the hole (or the last surviving line when the hole is at EOF).
        let at = if prefix < after.len() {
            prefix + 1
        } else {
            after.len().max(1)
        };
        return build_affected_region([RegionDelta::Deletion { at }]);
    }
    AffectedRegion::insertion(prefix + 1, after_mid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_spans_coalesce() {
        let region = build_affected_region([
            RegionDelta::OutputSpan { start: 2, count: 2 },
            RegionDelta::OutputSpan { start: 4, count: 1 },
        ]);
        assert_eq!(region.ranges, vec![LineRange::new(2, 4)]);
    }

    #[test]
    fn pure_insertion_into_empty_file() {
        let region = affected_from_line_diff(&[], &["a".into(), "b".into()]);
        assert_eq!(region.ranges, vec![LineRange::new(1, 2)]);
    }

    #[test]
    fn deletion_to_empty_has_no_rows() {
        let region = affected_from_line_diff(&["a".into()], &[]);
        assert!(region.is_empty());
    }

    #[test]
    fn middle_replacement_marks_new_rows() {
        let before = vec!["a".into(), "b".into(), "c".into()];
        let after = vec!["a".into(), "B".into(), "C".into(), "c".into()];
        let region = affected_from_line_diff(&before, &after);
        assert_eq!(region.ranges, vec![LineRange::new(2, 3)]);
    }
}
