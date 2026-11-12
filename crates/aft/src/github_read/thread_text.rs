//! Agent-visible review-thread wording is centralized here for review.

use super::model::{GithubComment, GithubDocument};

pub(super) const UNAVAILABLE: &str =
    "Review thread state couldn't be read; unresolved threads may still block the merge.\n";
pub(super) const STALE: &str = "Review thread state may be stale in this cached copy.\n";

pub(super) fn summary(document: &GithubDocument) -> String {
    let Some(state) = &document.review_threads else {
        return UNAVAILABLE.to_string();
    };
    let resolved = state
        .threads
        .iter()
        .filter(|thread| thread.is_resolved)
        .count();
    let open = state.threads.len() - resolved;
    let partial = if state.omitted_threads > 0 {
        " (partial)"
    } else {
        ""
    };
    let mut text = format!("Review threads: {open} open, {resolved} resolved{partial}\n");
    if state.omitted_threads > 0 {
        text.push_str(&format!(
            "{} review threads omitted; their state is unknown.\n",
            state.omitted_threads
        ));
    }
    if state.omitted_comment_ids > 0 {
        text.push_str(&format!(
            "{} review-thread comment IDs omitted; some comments' thread state is unknown.\n",
            state.omitted_comment_ids
        ));
    }
    text
}

pub(super) fn comment(document: &GithubDocument, comment: &GithubComment) -> String {
    let thread = document.review_threads.as_ref().and_then(|state| {
        let id = comment.id.as_ref()?;
        state
            .threads
            .iter()
            .find(|thread| thread.comment_ids.contains(id))
    });
    let Some(thread) = thread else {
        // A failed lookup already has one document-level warning; retain old comment text.
        return if document.review_threads.is_none() {
            String::new()
        } else {
            "Review thread: unknown (state and ID unavailable)\n\n".to_string()
        };
    };
    let state = if thread.is_resolved {
        "resolved"
    } else {
        "open"
    };
    let outdated = if thread.is_outdated {
        " (outdated)"
    } else {
        ""
    };
    format!("Review thread: {} — {state}{outdated}\n\n", thread.id)
}
