use crate::search_index::{GrepResult, IndexStatus, WalkBound};

/// A note spelling out what an incomplete fallback walk left unsearched and
/// how to get an exhaustive answer, or `None` when the walk reached every
/// file (or no walk ran).
pub(crate) fn walk_coverage_note(result: &GrepResult) -> Option<String> {
    let bound = result.walk_bound?;
    let files = result.files_read_directly;
    let index = match result.index_status {
        IndexStatus::Ready => "index ready",
        IndexStatus::Building => "index building",
        IndexStatus::Fallback => "index not used",
        IndexStatus::Disabled => "index disabled",
    };
    let exhaustive = if result.index_status == IndexStatus::Building {
        "For an exhaustive search, narrow with path or include so the walk reaches every file, or retry once the index is ready (an index search reads every indexed file)."
    } else {
        "For an exhaustive search, narrow with path or include so the walk reaches every file."
    };
    let checked = match bound {
        WalkBound::FileLimit(limit) => format!(
            "checked {files} of more than {limit} files ({index}); the fallback walk stops at {limit} files, so files past that limit were not searched and matches may be missing."
        ),
        WalkBound::TimeBudget => format!(
            "checked {} ({index}); the fallback walk ran out of its time budget before reaching every file, so files it did not reach were not searched and matches may be missing.",
            plural(files, "file")
        ),
        WalkBound::Cancelled => format!(
            "checked {} ({index}); the request was cancelled before the fallback walk reached every file, so matches may be missing.",
            plural(files, "file")
        ),
    };
    Some(format!("({checked} {exhaustive})"))
}

pub(crate) fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}
