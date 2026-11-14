//! Glob list surface adapter.
//!
//! Governs the `payload.files` list surface:
//! - Unit: files
//! - Narrow: path
//! - Causes:
//!   - `time_limit` (Bounding): the fallback walk's time budget expired
//!   - `walk` (Bounding): the walk was otherwise truncated or skipped a mount
//!   - `cap` (Selecting): `DEFAULT_MAX_RESULTS`, `MAX_DISPLAY_FILES_PER_DIRECTORY`, `MAX_DISPLAY_DIRECTORIES`

use crate::list_envelope::{ListEnvelope, Reason, Total, Unit};

pub const GLOB_LIST_ID: &str = "payload.files";
pub const GLOB_UNIT: Unit = Unit::Files;
pub const GLOB_NARROW: &[&str] = &["path"];

pub const DEFAULT_MAX_RESULTS: usize = 100;
pub const MAX_DISPLAY_FILES_PER_DIRECTORY: usize = 5;
pub const MAX_DISPLAY_DIRECTORIES: usize = 6;

/// Builds the `ListEnvelope` for a glob response when truncated, or returns `None` if complete.
///
/// Parameters:
/// - `shown`: rendered file count after display-only selectors (from the seam `rendered_glob_file_count`)
/// - `total`: discovered matching paths, excluding candidates known to be missing or denied
/// - `executor_capped`: true if the page/refill window left candidates unchecked
/// - `walk_truncated`: true if directory traversal stopped early
/// - `skipped_foreign_mounts`: count of foreign filesystem mounts skipped
pub fn build_glob_envelope(
    shown: usize,
    total: usize,
    executor_capped: bool,
    walk_truncated: bool,
    skipped_foreign_mounts: usize,
) -> Option<ListEnvelope> {
    build_glob_envelope_with_time_limit(
        shown,
        total,
        executor_capped,
        walk_truncated,
        false,
        skipped_foreign_mounts,
    )
}

/// Builds a glob envelope that can distinguish an exhausted time budget from
/// other reasons a filesystem walk stopped early.
pub fn build_glob_envelope_with_time_limit(
    shown: usize,
    total: usize,
    executor_capped: bool,
    walk_truncated: bool,
    time_limit_reached: bool,
    skipped_foreign_mounts: usize,
) -> Option<ListEnvelope> {
    let mut causes = Vec::new();

    let has_walk = walk_truncated || skipped_foreign_mounts > 0;
    if time_limit_reached {
        causes.push(Reason::TimeLimit);
    }
    if skipped_foreign_mounts > 0 || (walk_truncated && !time_limit_reached) {
        causes.push(Reason::Walk);
    }

    let has_cap = executor_capped || shown < total;
    if has_cap {
        causes.push(Reason::Cap);
    }

    if causes.is_empty() {
        return None;
    }

    let total_bound = if has_walk || executor_capped {
        Total::AtLeast(total)
    } else {
        Total::Exact(total)
    };
    Some(ListEnvelope::new(
        shown,
        total_bound,
        GLOB_UNIT,
        causes,
        GLOB_NARROW,
    ))
}
