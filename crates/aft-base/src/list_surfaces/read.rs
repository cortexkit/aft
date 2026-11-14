//! Directory-read list surface adapter.

use crate::list_envelope::{ListEnvelope, Reason, Total, Unit};

/// Distinguish incomplete enumeration from selecting a page of counted entries.
pub fn build_directory_envelope(
    shown: usize,
    total: usize,
    enumeration_cut: bool,
) -> Option<ListEnvelope> {
    let mut causes = Vec::new();
    if enumeration_cut {
        causes.push(Reason::Walk);
    }
    if shown < total {
        causes.push(Reason::Cap);
    }
    if causes.is_empty() {
        return None;
    }
    Some(ListEnvelope::new(
        shown,
        if enumeration_cut {
            Total::AtLeast(total)
        } else {
            Total::Exact(total)
        },
        Unit::Items,
        causes,
        &["path", "offset", "limit"],
    ))
}
