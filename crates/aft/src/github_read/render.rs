use std::cmp::Ordering;

use super::bot_compress::{compress_discussion_body, compress_document_body, structural_strip};
use super::fetch::GithubReadError;
use super::model::{
    GithubComment, GithubDocument, GithubDocumentKind, GithubReaction, GithubReview,
    GithubReviewCommentSection, GithubTimelineEvent,
};
use super::resource::{GithubCommentSelector, GithubResource};

/// The maximum number of newest comments rendered for an issue or one pull
/// request review-comment section. Older comments remain available on GitHub
/// and are disclosed in the canonical document instead of silently dropped.
pub const MAX_RENDERED_COMMENTS_PER_SECTION: usize = 50;

/// Render the complete, transport-independent GitHub document once in Rust.
///
/// Selector handling belongs after this function. The renderer purposefully
/// never sees a vision capability or an attachment downloader, so its bytes are
/// identical for text-only and vision-capable callers.
pub fn render_document(document: &GithubDocument) -> String {
    let resource = GithubResource {
        kind: document.resource_kind(),
        number: document.number,
        repository: Some(document.repository.clone()),
        comment_selector: None,
    };
    render_document_for_resource(document, &resource)
        .expect("a whole-document render has no fallible selector")
}

pub(super) fn render_document_for_resource(
    document: &GithubDocument,
    resource: &GithubResource,
) -> Result<String, GithubReadError> {
    if let Some(selector) = &resource.comment_selector {
        return render_selected_discussion(document, selector);
    }

    let mut output = String::new();
    let resource_label = match document.kind {
        GithubDocumentKind::Issue => "Issue",
        GithubDocumentKind::PullRequest => "Pull request",
    };
    output.push_str(&format!(
        "# {resource_label} #{}: {}\n\n",
        document.number, document.title
    ));
    output.push_str(&format!("Repository: {}\n", document.repository));
    output.push_str(&format!("State: {}\n", document.state));
    if let Some(author) = nonempty(&document.author) {
        output.push_str(&format!("Author: {}\n", format_author(author)));
    }
    if let Some(created_at) = nonempty(&document.created_at) {
        output.push_str(&format!("Created: {created_at}\n"));
    }
    if let Some(updated_at) = nonempty(&document.updated_at) {
        output.push_str(&format!("Updated: {updated_at}\n"));
    }
    if !document.labels.is_empty() {
        output.push_str(&format!("Labels: {}\n", document.labels.join(", ")));
    }
    if !document.assignees.is_empty() {
        output.push_str(&format!(
            "Assignees: {}\n",
            document
                .assignees
                .iter()
                .map(|assignee| format_author(assignee))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(milestone) = nonempty(&document.milestone) {
        output.push_str(&format!("Milestone: {milestone}\n"));
    }
    if let Some(reactions) = render_reactions(&document.reactions) {
        output.push_str(&format!("Reactions: {reactions}\n"));
    }

    output.push_str("\n## Body\n\n");
    append_body(&mut output, &compress_document_body(&document.body).body);

    let discussion = discussion_items(document);
    render_comments(
        &mut output,
        "Comments",
        &document.comments,
        document.comments_total_count,
        document.minimized_comments_count,
        &discussion,
        resource,
    );

    if document.kind == GithubDocumentKind::PullRequest {
        render_files(&mut output, document);
        render_reviews(&mut output, document, &discussion, resource);
        render_review_comment_sections(
            &mut output,
            &document.review_comment_sections,
            &discussion,
            resource,
        );
    }
    render_timeline(&mut output, &discussion);

    output.push_str(&format!(
        "Discussion drill-down: {}/comments/<sel> (for example 3, 3-5, or 3,7).\n\n",
        resource.base_spelling()
    ));
    Ok(output)
}

fn render_comments(
    output: &mut String,
    heading: &str,
    comments: &[GithubComment],
    total_count: Option<usize>,
    supplied_minimized_count: Option<usize>,
    discussion: &[DiscussionItem<'_>],
    resource: &GithubResource,
) {
    if comments.is_empty()
        && total_count.unwrap_or(0) == 0
        && supplied_minimized_count.unwrap_or(0) == 0
    {
        return;
    }
    output.push_str(&format!("\n## {heading}\n\n"));
    let displayed = newest_comments(comments);
    let total_count = total_count.unwrap_or(comments.len()).max(comments.len());
    let omitted = total_count.saturating_sub(displayed.len());
    if omitted > 0 {
        output.push_str(&format!("{omitted} earlier comments omitted\n\n"));
    }
    for comment in displayed {
        if let Some(ordinal) = discussion_ordinal_for_comment(discussion, comment, false) {
            render_default_discussion_item(
                output,
                ordinal,
                comment.author.as_deref(),
                comment.created_at.as_deref(),
                None,
                &comment.body,
                resource,
            );
        }
    }
    let observed_minimized = comments.iter().filter(|comment| comment.minimized).count();
    let minimized = supplied_minimized_count
        .unwrap_or(observed_minimized)
        .max(observed_minimized);
    if minimized > 0 {
        output.push_str(&format!("Minimized comments: {minimized}\n\n"));
    }
}

fn render_files(output: &mut String, document: &GithubDocument) {
    if document.files.is_empty() {
        return;
    }
    output.push_str("\n## Files\n\n");
    for file in &document.files {
        output.push_str("- `");
        output.push_str(&file.path);
        output.push('`');
        if file.additions.is_some() || file.deletions.is_some() {
            output.push_str(&format!(
                " (+{} -{})",
                file.additions.unwrap_or(0),
                file.deletions.unwrap_or(0)
            ));
        }
        if let Some(status) = nonempty(&file.status) {
            output.push_str(&format!(" [{status}]"));
        }
        output.push('\n');
    }
}

fn render_reviews(
    output: &mut String,
    document: &GithubDocument,
    discussion: &[DiscussionItem<'_>],
    resource: &GithubResource,
) {
    if !document.reviews.iter().any(review_is_visible) {
        return;
    }
    output.push_str("\n## Reviews\n\n");
    for review in &document.reviews {
        let Some(ordinal) = discussion_ordinal_for_review(discussion, review) else {
            continue;
        };
        render_default_discussion_item(
            output,
            ordinal,
            review.author.as_deref(),
            review.submitted_at.as_deref(),
            review.state.as_deref(),
            &review.body,
            resource,
        );
    }
}

fn render_review_comment_sections(
    output: &mut String,
    sections: &[GithubReviewCommentSection],
    discussion: &[DiscussionItem<'_>],
    resource: &GithubResource,
) {
    if !sections.iter().any(|section| {
        section.comments.iter().any(comment_is_visible)
            || section.comments_total_count.unwrap_or(0) > section.comments.len()
            || section.minimized_comments_count.unwrap_or(0) > 0
    }) {
        return;
    }
    output.push_str("\n## Review comments\n\n");
    for section in sections {
        let displayed = newest_comments(&section.comments);
        let total_count = section
            .comments_total_count
            .unwrap_or(section.comments.len())
            .max(section.comments.len());
        let omitted = total_count.saturating_sub(displayed.len());
        if omitted > 0 {
            let author = section
                .author
                .as_deref()
                .map(format_author)
                .unwrap_or_else(|| "unknown".to_string());
            output.push_str(&format!(
                "{omitted} earlier comments omitted from {author}'s review\n\n"
            ));
        }
        for comment in displayed {
            if let Some(ordinal) = discussion_ordinal_for_comment(discussion, comment, true) {
                render_default_discussion_item(
                    output,
                    ordinal,
                    comment.author.as_deref(),
                    comment.created_at.as_deref(),
                    None,
                    &comment.body,
                    resource,
                );
            }
        }
        let observed_minimized = section
            .comments
            .iter()
            .filter(|comment| comment.minimized)
            .count();
        let minimized = section
            .minimized_comments_count
            .unwrap_or(observed_minimized)
            .max(observed_minimized);
        if minimized > 0 {
            output.push_str(&format!("Minimized comments: {minimized}\n\n"));
        }
    }
}

fn render_timeline(output: &mut String, discussion: &[DiscussionItem<'_>]) {
    let events = discussion
        .iter()
        .enumerate()
        .filter_map(|(index, item)| item.event().map(|event| (index + 1, event)));
    let mut rendered_any = false;
    for (ordinal, event) in events {
        if !rendered_any {
            output.push_str("\n## Timeline\n\n");
            rendered_any = true;
        }
        render_item_heading(
            output,
            ordinal,
            event.actor.as_deref(),
            event.created_at.as_deref(),
        );
        render_timeline_event_body(output, event);
    }
}

fn render_default_discussion_item(
    output: &mut String,
    ordinal: usize,
    author: Option<&str>,
    date: Option<&str>,
    state: Option<&str>,
    body: &str,
    resource: &GithubResource,
) {
    let compressed = compress_discussion_body(author, body);
    if compressed.body.is_empty() {
        return;
    }
    render_item_heading(output, ordinal, author, date);
    if let Some(state) = state.filter(|state| !state.is_empty()) {
        output.push_str(&format!("State: {state}\n\n"));
    }
    append_body(output, &compressed.body);
    if compressed.compressed {
        output.push_str(&format!(
            "[compressed; full: {}/comments/{ordinal}]\n\n",
            resource.base_spelling()
        ));
    }
}

fn render_selected_discussion(
    document: &GithubDocument,
    selector: &GithubCommentSelector,
) -> Result<String, GithubReadError> {
    let items = discussion_items(document);
    if let Some(ordinal) = selector.first_out_of_range(items.len()) {
        let valid_range = if items.is_empty() {
            "empty".to_string()
        } else {
            format!("1-{}", items.len())
        };
        return Err(GithubReadError::InvalidCommentSelector(format!(
            "discussion ordinal {ordinal} is out of range; valid range is {valid_range}"
        )));
    }

    let mut output = String::new();
    for (index, item) in items.into_iter().enumerate() {
        let ordinal = index + 1;
        if !selector.contains(ordinal) {
            continue;
        }
        render_item_heading(&mut output, ordinal, item.author(), item.date());
        if let Some(event) = item.event() {
            render_timeline_event_body(&mut output, event);
            continue;
        }
        if let Some(state) = item.state().filter(|state| !state.is_empty()) {
            output.push_str(&format!("State: {state}\n\n"));
        }
        append_body(
            &mut output,
            &structural_strip(item.body().unwrap_or_default()),
        );
    }
    Ok(output)
}

#[derive(Clone, Copy)]
enum DiscussionItemKind<'a> {
    Comment(&'a GithubComment),
    Review(&'a GithubReview),
    ReviewComment(&'a GithubComment),
    Event(&'a GithubTimelineEvent),
}

#[derive(Clone, Copy)]
struct DiscussionItem<'a> {
    kind: DiscussionItemKind<'a>,
}

impl<'a> DiscussionItem<'a> {
    fn author(self) -> Option<&'a str> {
        match self.kind {
            DiscussionItemKind::Comment(comment) | DiscussionItemKind::ReviewComment(comment) => {
                comment.author.as_deref()
            }
            DiscussionItemKind::Review(review) => review.author.as_deref(),
            DiscussionItemKind::Event(event) => event.actor.as_deref(),
        }
    }

    fn date(self) -> Option<&'a str> {
        match self.kind {
            DiscussionItemKind::Comment(comment) | DiscussionItemKind::ReviewComment(comment) => {
                comment.created_at.as_deref()
            }
            DiscussionItemKind::Review(review) => review.submitted_at.as_deref(),
            DiscussionItemKind::Event(event) => event.created_at.as_deref(),
        }
    }

    fn state(self) -> Option<&'a str> {
        match self.kind {
            DiscussionItemKind::Review(review) => review.state.as_deref(),
            _ => None,
        }
    }

    fn body(self) -> Option<&'a str> {
        match self.kind {
            DiscussionItemKind::Comment(comment) | DiscussionItemKind::ReviewComment(comment) => {
                Some(&comment.body)
            }
            DiscussionItemKind::Review(review) => Some(&review.body),
            DiscussionItemKind::Event(_) => None,
        }
    }

    fn event(self) -> Option<&'a GithubTimelineEvent> {
        match self.kind {
            DiscussionItemKind::Event(event) => Some(event),
            _ => None,
        }
    }
}

fn discussion_items(document: &GithubDocument) -> Vec<DiscussionItem<'_>> {
    let mut items = Vec::new();
    for comment in newest_comments(&document.comments) {
        if comment_is_visible(comment) {
            items.push(DiscussionItem {
                kind: DiscussionItemKind::Comment(comment),
            });
        }
    }
    if document.kind == GithubDocumentKind::PullRequest {
        for review in &document.reviews {
            if review_is_visible(review) {
                items.push(DiscussionItem {
                    kind: DiscussionItemKind::Review(review),
                });
            }
        }
        for section in &document.review_comment_sections {
            for comment in newest_comments(&section.comments) {
                if comment_is_visible(comment) {
                    items.push(DiscussionItem {
                        kind: DiscussionItemKind::ReviewComment(comment),
                    });
                }
            }
        }
    }
    if document.timeline.is_empty() {
        return items;
    }
    for event in &document.timeline {
        items.push(DiscussionItem {
            kind: DiscussionItemKind::Event(event),
        });
    }
    items.sort_by(|left, right| left.date().unwrap_or("").cmp(right.date().unwrap_or("")));
    items
}

fn discussion_ordinal_for_comment(
    discussion: &[DiscussionItem<'_>],
    comment: &GithubComment,
    review_comment: bool,
) -> Option<usize> {
    discussion
        .iter()
        .position(|item| {
            matches!(
                item.kind,
                DiscussionItemKind::ReviewComment(candidate)
                    if review_comment && std::ptr::eq(candidate, comment)
            ) || matches!(
                item.kind,
                DiscussionItemKind::Comment(candidate)
                    if !review_comment && std::ptr::eq(candidate, comment)
            )
        })
        .map(|index| index + 1)
}

fn discussion_ordinal_for_review(
    discussion: &[DiscussionItem<'_>],
    review: &GithubReview,
) -> Option<usize> {
    discussion
        .iter()
        .position(|item| matches!(item.kind, DiscussionItemKind::Review(candidate) if std::ptr::eq(candidate, review)))
        .map(|index| index + 1)
}

/// Render the concise GitHub discussion index used by `aft_outline`.
pub fn render_outline_for_resource(document: &GithubDocument, resource: &GithubResource) -> String {
    let items = discussion_items(document);
    let mut lines = vec![format!("#{} {}", document.number, document.title)];
    let state = if document
        .timeline
        .iter()
        .any(|event| event.event.eq_ignore_ascii_case("merged"))
    {
        "merged".to_string()
    } else {
        document.state.to_ascii_lowercase()
    };
    let author = document
        .author
        .as_deref()
        .map(format_author)
        .unwrap_or_else(|| "@unknown".to_string());
    let created = document.created_at.as_deref().unwrap_or("unknown");
    let updated = document.updated_at.as_deref().unwrap_or("unknown");
    let labels = document.labels.join(",");
    let mut metadata = format!(
        "state={state} author={author} created={created} updated={updated} labels=[{labels}]"
    );
    if document.kind == GithubDocumentKind::PullRequest {
        let base = document.base_ref_name.as_deref().unwrap_or("?");
        let head = document.head_ref_name.as_deref().unwrap_or("?");
        let additions = document
            .files
            .iter()
            .map(|file| file.additions.unwrap_or(0))
            .sum::<u64>();
        let deletions = document
            .files
            .iter()
            .map(|file| file.deletions.unwrap_or(0))
            .sum::<u64>();
        let decision = document
            .review_decision
            .as_deref()
            .unwrap_or("unknown")
            .to_ascii_lowercase();
        metadata.push_str(&format!(
            " {base}<-{head} +{additions}/-{deletions} files={} review={decision}",
            document.files.len()
        ));
    }
    lines.push(metadata);

    const OUTLINE_ITEM_CAP: usize = 200;
    let omitted = items.len().saturating_sub(OUTLINE_ITEM_CAP);
    let first_count = if omitted > 0 { 20 } else { items.len() };
    for (index, item) in items.iter().take(first_count).enumerate() {
        lines.push(render_outline_item(index + 1, item));
    }
    if omitted > 0 {
        let first_omitted = first_count + 1;
        let last_omitted = items.len() - 180;
        lines.push(format!(
            "… ({omitted} omitted; read {}/comments/{first_omitted}-{last_omitted})",
            resource.base_spelling()
        ));
        for (index, item) in items.iter().enumerate().skip(items.len() - 180) {
            lines.push(render_outline_item(index + 1, item));
        }
    }
    lines.push(format!(
        "Zoom items: aft_zoom {} <k>[,k..] · full: read {}",
        resource.base_spelling(),
        resource.base_spelling()
    ));
    format!("{}\n", lines.join("\n"))
}

fn render_outline_item(ordinal: usize, item: &DiscussionItem<'_>) -> String {
    let kind = match item.kind {
        DiscussionItemKind::Comment(_) => "comment".to_string(),
        DiscussionItemKind::Review(_) => format!(
            "review({})",
            item.state().unwrap_or("unknown").to_ascii_lowercase()
        ),
        DiscussionItemKind::ReviewComment(comment) => format!(
            "review-comment({}:{})",
            comment.path.as_deref().unwrap_or("unknown"),
            comment
                .line
                .map(|line| line.to_string())
                .unwrap_or_else(|| "?".to_string())
        ),
        DiscussionItemKind::Event(event) => format!("event({})", event.event),
    };
    let author = item
        .author()
        .map(format_author)
        .unwrap_or_else(|| "@unknown".to_string());
    let date = outline_timestamp(item.date().unwrap_or("unknown"));
    let body = item.body().map(single_line_excerpt).unwrap_or_else(|| {
        single_line_excerpt(&timeline_event_payload(item.event().expect("event item")))
    });
    format!("[{ordinal}] {kind} {author} {date} · {body}")
}

fn outline_timestamp(timestamp: &str) -> String {
    timestamp
        .strip_suffix(":00Z")
        .map(|value| value.replace('T', " "))
        .unwrap_or_else(|| timestamp.replace('T', " "))
}

fn single_line_excerpt(value: &str) -> String {
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_CHARS: usize = 80;
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let prefix = text
        .chars()
        .take(MAX_CHARS.saturating_sub(1))
        .collect::<String>();
    format!("{prefix}…")
}

fn render_timeline_event_body(output: &mut String, event: &GithubTimelineEvent) {
    output.push_str(&format!("Event: {}\n\n", event.event));
    append_body(output, &timeline_event_payload(event));
}

fn timeline_event_payload(event: &GithubTimelineEvent) -> String {
    let mut payload = Vec::new();
    if let Some(label) = &event.label {
        payload.push(format!("Label: {label}"));
    }
    if let Some(assignee) = &event.assignee {
        payload.push(format!("Assignee: {}", format_author(assignee)));
    }
    if let Some(milestone) = &event.milestone {
        payload.push(format!("Milestone: {milestone}"));
    }
    if event.rename_from.is_some() || event.rename_to.is_some() {
        payload.push(format!(
            "Renamed: {} -> {}",
            event.rename_from.as_deref().unwrap_or("unknown"),
            event.rename_to.as_deref().unwrap_or("unknown")
        ));
    }
    if let Some(reviewer) = &event.requested_reviewer {
        payload.push(format!("Requested reviewer: {}", format_author(reviewer)));
    }
    if let Some(commit_id) = &event.commit_id {
        payload.push(format!("Merge commit: {commit_id}"));
    }
    if payload.is_empty() {
        event.event.clone()
    } else {
        payload.join("; ")
    }
}

fn review_is_visible(review: &GithubReview) -> bool {
    discussion_body_is_visible(review.author.as_deref(), &review.body)
}

fn comment_is_visible(comment: &GithubComment) -> bool {
    discussion_body_is_visible(comment.author.as_deref(), &comment.body)
}

fn discussion_body_is_visible(author: Option<&str>, body: &str) -> bool {
    !compress_discussion_body(author, body).body.is_empty()
}

fn render_item_heading(
    output: &mut String,
    ordinal: usize,
    author: Option<&str>,
    date: Option<&str>,
) {
    let author = author
        .map(format_author)
        .unwrap_or_else(|| "unknown".to_string());
    let date = date.unwrap_or("unknown date");
    output.push_str(&format!("### [{ordinal}] {author} · {date}\n\n"));
}

fn newest_comments(comments: &[GithubComment]) -> Vec<&GithubComment> {
    let mut comments: Vec<_> = comments.iter().collect();
    comments.sort_by(|left, right| compare_timestamp(&left.created_at, &right.created_at));
    let drop_count = comments
        .len()
        .saturating_sub(MAX_RENDERED_COMMENTS_PER_SECTION);
    comments.into_iter().skip(drop_count).collect()
}

fn compare_timestamp(left: &Option<String>, right: &Option<String>) -> Ordering {
    left.as_deref()
        .unwrap_or("")
        .cmp(right.as_deref().unwrap_or(""))
}

fn render_reactions(reactions: &[GithubReaction]) -> Option<String> {
    let rendered: Vec<_> = reactions
        .iter()
        .filter(|reaction| reaction.count > 0)
        .map(|reaction| {
            format!(
                "{} x{}",
                reaction_display(&reaction.content),
                reaction.count
            )
        })
        .collect();
    (!rendered.is_empty()).then(|| rendered.join(", "))
}

fn reaction_display(reaction: &str) -> &str {
    match reaction {
        "THUMBS_UP" | "+1" => "+1",
        "THUMBS_DOWN" | "-1" => "-1",
        "LAUGH" => "laugh",
        "HOORAY" => "hooray",
        "CONFUSED" => "confused",
        "HEART" => "heart",
        "ROCKET" => "rocket",
        "EYES" => "eyes",
        other => other,
    }
}

fn format_author(author: &str) -> String {
    if author.starts_with('@') {
        author.to_string()
    } else {
        format!("@{author}")
    }
}

fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.is_empty())
}

fn append_body(output: &mut String, body: &str) {
    if body.is_empty() {
        return;
    }
    output.push_str(body.trim_end());
    output.push_str("\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github_read::model::{GithubDocument, GithubDocumentKind, GithubTimelineEvent};
    use crate::github_read::GithubResourceKind;

    #[test]
    fn renderer_keeps_full_human_body_caps_comments_and_omits_comment_reactions() {
        let mut document = GithubDocument {
            repository: "owner/repo".to_string(),
            kind: GithubDocumentKind::Issue,
            number: 7,
            title: "Fixture".to_string(),
            state: "OPEN".to_string(),
            author: Some("octo".to_string()),
            body: "body with https://user-images.githubusercontent.com/example.png".to_string(),
            reactions: vec![GithubReaction {
                content: "THUMBS_UP".to_string(),
                count: 2,
            }],
            ..GithubDocument::default()
        };
        document.comments = (0..51)
            .map(|number| GithubComment {
                author: Some("commenter".to_string()),
                body: format!("comment {number}"),
                created_at: Some(format!("2026-01-{:02}T00:00:00Z", number.min(28))),
                minimized: number == 0,
                ..GithubComment::default()
            })
            .collect();
        document.comments_total_count = Some(51);

        let text = render_document(&document);
        assert!(text.contains("Repository: owner/repo"));
        assert!(text.contains("Reactions: +1 x2"));
        assert!(text.contains("1 earlier comments omitted"));
        assert!(text.contains("Minimized comments: 1"));
        assert!(!text.contains("comment 0\n"));
        assert!(text.contains("### [1] @commenter"));
        assert!(text.contains("### [50] @commenter"));
        assert_eq!(text.matches("/comments/<sel>").count(), 1);
    }

    #[test]
    fn timeline_events_share_ordinals_between_outline_read_and_selector() {
        let document = GithubDocument {
            repository: "cortexkit/aft".to_string(),
            kind: GithubDocumentKind::PullRequest,
            number: 999,
            title: "Timeline fixture".to_string(),
            state: "OPEN".to_string(),
            author: Some("author".to_string()),
            created_at: Some("2026-09-03T05:00:00Z".to_string()),
            updated_at: Some("2026-09-03T07:00:00Z".to_string()),
            comments: vec![
                GithubComment {
                    author: Some("commenter".to_string()),
                    body: "First comment".to_string(),
                    created_at: Some("2026-09-03T05:10:00Z".to_string()),
                    ..GithubComment::default()
                },
                GithubComment {
                    author: Some("commenter".to_string()),
                    body: "Last comment".to_string(),
                    created_at: Some("2026-09-03T06:50:00Z".to_string()),
                    ..GithubComment::default()
                },
            ],
            timeline: vec![GithubTimelineEvent {
                actor: Some("aft-alfonso[bot]".to_string()),
                event: "closed".to_string(),
                created_at: Some("2026-09-03T06:57:00Z".to_string()),
                commit_id: Some("0123456789abcdef".to_string()),
                ..GithubTimelineEvent::default()
            }],
            ..GithubDocument::default()
        };
        let resource = GithubResource {
            kind: GithubResourceKind::PullRequest,
            number: 999,
            repository: Some("cortexkit/aft".to_string()),
            comment_selector: None,
        };

        let outline = render_outline_for_resource(&document, &resource);
        let rendered = render_document_for_resource(&document, &resource).expect("render read");
        let selected = render_document_for_resource(
            &document,
            &GithubResource {
                comment_selector: Some(GithubCommentSelector::parse("3").expect("parse selector")),
                ..resource.clone()
            },
        )
        .expect("render selected event");

        assert!(outline.contains("[3] event(closed) @aft-alfonso[bot] 2026-09-03 06:57"));
        assert!(rendered.contains("## Timeline\n\n### [3] @aft-alfonso[bot]"));
        assert!(selected.contains("Event: closed"));
        assert!(selected.contains("0123456789abcdef"));
    }

    #[test]
    fn outline_cap_keeps_first_twenty_last_one_hundred_eighty_and_escape_hatch() {
        let mut document = GithubDocument {
            repository: "owner/repo".to_string(),
            kind: GithubDocumentKind::Issue,
            number: 7,
            title: "Cap fixture".to_string(),
            state: "OPEN".to_string(),
            ..GithubDocument::default()
        };
        document.timeline = (1..=250)
            .map(|number| GithubTimelineEvent {
                actor: Some("maintainer".to_string()),
                event: "labeled".to_string(),
                created_at: Some(format!("2026-01-01T00:{number:02}:00Z")),
                label: Some(format!("label {number}")),
                ..GithubTimelineEvent::default()
            })
            .collect();
        let resource = GithubResource {
            kind: GithubResourceKind::Issue,
            number: 7,
            repository: Some("owner/repo".to_string()),
            comment_selector: None,
        };

        let outline = render_outline_for_resource(&document, &resource);
        assert!(outline.contains("[20] event(labeled)"));
        assert!(outline.contains("… (50 omitted; read issue://owner/repo/7/comments/21-70)"));
        assert!(outline.contains("[71] event(labeled)"));
        assert!(outline.contains("[250] event(labeled)"));
        assert!(!outline.contains("[21] event(labeled)"));
    }
}
