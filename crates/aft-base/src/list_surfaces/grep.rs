//! Grep list surface truncation adapter.
//!
//! Produces the truncation envelope for grep match lists (`payload.matches`),
//! accounting for directory walk boundaries (such as search timeouts or skipped
//! filesystem mounts), match caps (such as the page size, `offset` paging,
//! or the output byte budget), and lines grep counted instead of printing
//! because of their file's kind (lockfiles, logs, generated files, ...).

use crate::list_envelope::{ListEnvelope, Reason, Total, Unit};
use serde_json::Value;

pub const COMMAND: &str = "grep";
pub const LIST_ID: &str = "payload.matches";
pub const UNIT: Unit = Unit::Rows;
pub const NARROW: &[&str] = &["offset", "path", "include"];

pub const IGNORED_LIST_ID: &str = "payload.ignored_entries";
pub const IGNORED_UNIT: Unit = Unit::Paths;
pub const IGNORED_NARROW: &[&str] = &["path"];

/// Ignore boundaries have their own list: the preview cap never changes the
/// match-row envelope, and an unfinished folder listing has no exact total.
pub fn build_ignored_entries_envelope(data: &Value) -> Option<ListEnvelope> {
    let shown = data.get("ignored_entries")?.as_array()?.len();
    let total = data.get("ignored_entries_total").and_then(Value::as_u64);
    let mut causes = Vec::new();
    if total.is_none() {
        causes.push(Reason::Walk);
    }
    if total.is_some_and(|total| total > shown as u64)
        || total.is_none() && shown == MAX_IGNORED_SCOPE_NAMES
    {
        causes.push(Reason::Cap);
    }
    if causes.is_empty() {
        return None;
    }
    let total = total.map_or(Total::AtLeast(shown), |total| Total::Exact(total as usize));
    Some(ListEnvelope::new(
        shown,
        total,
        IGNORED_UNIT,
        causes,
        IGNORED_NARROW,
    ))
}

/// Build the list truncation envelope for a grep response from its component parts.
///
/// Returns `None` if no truncation cause fired (the response is complete).
pub fn build_grep_envelope_from_parts(
    shown: usize,
    total_matches: usize,
    matches_count: usize,
    truncated: bool,
    walk_truncated: bool,
    skipped_foreign_mounts: usize,
) -> Option<ListEnvelope> {
    build_grep_envelope_for_page(
        0,
        shown,
        total_matches,
        matches_count,
        truncated,
        walk_truncated,
        skipped_foreign_mounts,
    )
}

/// Build the list truncation envelope for one page of grep matches.
///
/// `offset` is how many matching lines earlier pages covered, `matches_count`
/// is how many matches this page held and `shown` how many of them were
/// printed (the output byte budget can stop rendering early). A page that
/// skipped earlier rows, left rows unprinted, or has rows after it is not the
/// complete list, so it gets a `cap` envelope.
pub fn build_grep_envelope_for_page(
    offset: usize,
    shown: usize,
    total_matches: usize,
    matches_count: usize,
    truncated: bool,
    walk_truncated: bool,
    skipped_foreign_mounts: usize,
) -> Option<ListEnvelope> {
    build_grep_envelope_with_counted(
        GrepPageCounts {
            offset,
            shown,
            total_matches,
            display_matches: total_matches,
            matches_count,
            counted_matches: 0,
        },
        truncated,
        walk_truncated,
        skipped_foreign_mounts,
    )
}

/// Row counts of one grep page.
#[derive(Debug, Clone, Copy)]
pub struct GrepPageCounts {
    /// Printed rows earlier pages covered.
    pub offset: usize,
    /// Rows this page printed.
    pub shown: usize,
    /// Every matching line in scope, counted ones included.
    pub total_matches: usize,
    /// Rows grep prints across all pages: every match except the lines left
    /// counted only.
    pub display_matches: usize,
    /// Rows this page held before the output byte budget applied.
    pub matches_count: usize,
    /// Matching lines counted, not printed, because of their file's kind.
    pub counted_matches: usize,
}

/// Build the list truncation envelope for one page of grep matches, where
/// some matching lines may be counted instead of printed. Paging cuts are
/// `cap`; lines counted because of their file's kind are `kind`, which ranks
/// below `cap`, so `kind` is the reason only when nothing else cut rows.
pub fn build_grep_envelope_with_counted(
    counts: GrepPageCounts,
    truncated: bool,
    walk_truncated: bool,
    skipped_foreign_mounts: usize,
) -> Option<ListEnvelope> {
    let GrepPageCounts {
        offset,
        shown,
        total_matches,
        display_matches,
        matches_count,
        counted_matches,
    } = counts;
    let walk_cause = walk_truncated || skipped_foreign_mounts > 0;
    let seen = offset.saturating_add(matches_count);
    let cap_cause = truncated || shown < matches_count || offset > 0 || display_matches > seen;
    let kind_cause = counted_matches > 0;

    if !walk_cause && !cap_cause && !kind_cause {
        return None;
    }

    let mut causes = Vec::new();
    if walk_cause {
        causes.push(Reason::Walk);
    }
    if cap_cause {
        causes.push(Reason::Cap);
    }
    if kind_cause {
        causes.push(Reason::Kind);
    }

    let total = if walk_cause {
        // Any traversal cut means the true total is unknown, making the count a lower bound.
        Total::AtLeast(total_matches.max(offset.saturating_add(shown)))
    } else if truncated {
        // When the executor hits its result cap, the match count is a lower bound.
        Total::AtLeast(total_matches.max(seen))
    } else {
        // When enumeration completed and only paging or the output budget cut
        // the printed rows, the total count is exact.
        Total::Exact(total_matches.max(seen))
    };

    Some(ListEnvelope::new(shown, total, UNIT, causes, NARROW))
}

/// Build the list truncation envelope for a grep response JSON payload.
///
/// Inspects the response payload for walk boundaries and match caps, deriving
/// the rendered match count from the formatter seam. Returns `None` if the
/// result is complete.
pub fn build_grep_envelope(data: &Value) -> Option<ListEnvelope> {
    let matches = data.get("matches").and_then(Value::as_array);
    let matches_count = matches.map(|m| m.len()).unwrap_or(0);
    let total_matches = data
        .get("total_matches")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(matches_count);
    let offset = data
        .get("offset")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(0);
    let truncated = data
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let walk_truncated = data
        .get("walk_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        // A scan stopped by its time budget left candidate files unread, the
        // same traversal cut as a walk stopped early: the total is a floor.
        || data
            .get("scan_deadline_reached")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let skipped_foreign_mounts = data
        .get("skipped_foreign_mounts")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(0);

    // Replies from before file kinds were counted carry neither field: every
    // match was printable and none was counted.
    let display_matches = data
        .get("display_matches")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(total_matches);
    let counted_matches = data
        .get("counted_matches")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(0);

    let shown = rendered_grep_match_count(data);

    build_grep_envelope_with_counted(
        GrepPageCounts {
            offset,
            shown,
            total_matches,
            display_matches,
            matches_count,
            counted_matches,
        },
        truncated,
        walk_truncated,
        skipped_foreign_mounts,
    )
}

/// Count of rendered match rows in a grep response.
///
/// `matches` holds the whole requested page; `rendered_matches` says how many
/// of those rows fit the output byte budget and were printed. Replies without
/// that field printed every row in `matches`.
pub fn rendered_grep_match_count(data: &Value) -> usize {
    let matches_len = data
        .get("matches")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    data.get("rendered_matches")
        .and_then(Value::as_u64)
        .map(|rendered| (rendered as usize).min(matches_len))
        .unwrap_or(matches_len)
}

/// Display names retained for an explicitly ignored search scope.
pub const MAX_IGNORED_SCOPE_NAMES: usize = 3;
