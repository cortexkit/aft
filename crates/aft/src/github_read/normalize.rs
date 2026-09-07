use std::fmt;

use serde_json::Value;
use url::Url;

use super::model::{
    GithubComment, GithubDocument, GithubDocumentKind, GithubPullRequestFile, GithubReaction,
    GithubReview, GithubReviewCommentSection, GithubTimelineEvent,
};
use super::resource::{GithubResource, GithubResourceKind};

/// A structured-response error. The fetch layer maps this into a typed,
/// redacted user-facing fetch failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizeError(pub String);

impl fmt::Display for NormalizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NormalizeError {}

/// Normalize `gh --json` or GraphQL JSON into the renderer's stable model.
///
/// The function accepts the ordinary GitHub CLI spelling as well as the
/// `data.repository.issueOrPullRequest` GraphQL envelope used by shims. It
/// never accepts display-oriented text; malformed structured data is an error.
pub fn normalize_structured_document(
    resource: &GithubResource,
    json: &Value,
) -> Result<GithubDocument, NormalizeError> {
    let root = document_value(json).ok_or_else(|| {
        NormalizeError("GitHub returned structured data without a resource document".to_string())
    })?;
    let repository = resolved_repository(json, root).ok_or_else(|| {
        NormalizeError(
            "GitHub structured response did not identify the resolved owner/repository".to_string(),
        )
    })?;
    let number = value_u64(root, "number").unwrap_or(resource.number);
    if number != resource.number {
        return Err(NormalizeError(format!(
            "GitHub returned resource #{number} for requested #{}",
            resource.number
        )));
    }

    let kind = match resource.kind {
        GithubResourceKind::Issue => GithubDocumentKind::Issue,
        GithubResourceKind::PullRequest => GithubDocumentKind::PullRequest,
    };
    let comments = comments_from(root.get("comments"));
    let comments_total_count = total_count(root.get("comments"));
    let minimized_comments_count = value_usize(root, "minimizedCommentsCount")
        .or_else(|| value_usize(root, "minimized_comments_count"));

    let mut document = GithubDocument {
        repository,
        kind,
        number,
        title: value_string(root, "title").unwrap_or_default(),
        state: value_string(root, "state").unwrap_or_else(|| "UNKNOWN".to_string()),
        author: actor_login(root.get("author")).or_else(|| value_string(root, "author")),
        created_at: value_string(root, "createdAt").or_else(|| value_string(root, "created_at")),
        updated_at: value_string(root, "updatedAt").or_else(|| value_string(root, "updated_at")),
        labels: labels_from(root.get("labels")),
        assignees: actors_from(root.get("assignees")),
        milestone: milestone_from(root.get("milestone")),
        body: value_string(root, "body").unwrap_or_default(),
        reactions: reactions_from(
            root.get("reactionGroups")
                .or_else(|| root.get("reaction_groups"))
                .or_else(|| root.get("reactions")),
        ),
        comments,
        comments_total_count,
        minimized_comments_count,
        files: Vec::new(),
        reviews: Vec::new(),
        review_comment_sections: Vec::new(),
        base_ref_name: None,
        head_ref_name: None,
        review_decision: None,
        timeline: Vec::new(),
    };

    if resource.kind == GithubResourceKind::PullRequest {
        document.files = files_from(root.get("files"));
        document.reviews = reviews_from(root.get("reviews"));
        document.base_ref_name =
            value_string(root, "baseRefName").or_else(|| value_string(root, "base_ref_name"));
        document.head_ref_name =
            value_string(root, "headRefName").or_else(|| value_string(root, "head_ref_name"));
        document.review_decision =
            value_string(root, "reviewDecision").or_else(|| value_string(root, "review_decision"));
        document.review_comment_sections = review_comment_sections_from(
            root.get("reviewCommentSections")
                .or_else(|| root.get("review_comment_sections")),
        );
        // Shims that retain review comments on each review do not need a second
        // top-level section field. Normalize that representation here too.
        if document.review_comment_sections.is_empty() {
            document.review_comment_sections = document
                .reviews
                .iter()
                .filter(|review| {
                    !review.comments.is_empty() || review.comments_total_count.is_some()
                })
                .map(|review| GithubReviewCommentSection {
                    author: review.author.clone(),
                    submitted_at: review.submitted_at.clone(),
                    comments: review.comments.clone(),
                    comments_total_count: review.comments_total_count,
                    minimized_comments_count: None,
                })
                .collect();
        }
    }

    Ok(document)
}

fn document_value(json: &Value) -> Option<&Value> {
    json.pointer("/data/repository/issueOrPullRequest")
        .or_else(|| json.pointer("/data/repository/pullRequest"))
        .or_else(|| json.pointer("/data/repository/issue"))
        .or_else(|| json.pointer("/data/resource"))
        .or_else(|| json.get("resource"))
        .or_else(|| json.as_object().map(|_| json))
}

fn resolved_repository(json: &Value, root: &Value) -> Option<String> {
    repository_from_value(root.get("repository"))
        .or_else(|| repository_from_value(json.pointer("/data/repository")))
        .or_else(|| repository_from_url(root.get("url").and_then(Value::as_str)))
        .or_else(|| repository_from_url(json.get("url").and_then(Value::as_str)))
}

fn repository_from_value(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(value) = value.as_str() {
        return normalized_repository(value);
    }
    for name in ["nameWithOwner", "name_with_owner", "fullName", "full_name"] {
        if let Some(repository) = value.get(name).and_then(Value::as_str) {
            return normalized_repository(repository);
        }
    }
    let owner = actor_login(value.get("owner"))?;
    let name = value.get("name").and_then(Value::as_str)?;
    normalized_repository(&format!("{owner}/{name}"))
}

fn repository_from_url(value: Option<&str>) -> Option<String> {
    let url = Url::parse(value?).ok()?;
    if url.scheme() != "https" || url.host_str()? != "github.com" {
        return None;
    }
    let mut segments = url.path_segments()?;
    let owner = segments.next()?;
    let repository = segments.next()?;
    normalized_repository(&format!("{owner}/{repository}"))
}

fn normalized_repository(value: &str) -> Option<String> {
    let mut parts = value.split('/');
    let owner = parts.next()?.trim();
    let repository = parts.next()?.trim();
    if parts.next().is_some() || !valid_component(owner) || !valid_component(repository) {
        return None;
    }
    Some(format!("{owner}/{repository}"))
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn collection_nodes(value: Option<&Value>) -> Vec<&Value> {
    match value {
        Some(Value::Array(values)) => values.iter().collect(),
        Some(Value::Object(values)) => values
            .get("nodes")
            .or_else(|| values.get("items"))
            .or_else(|| values.get("edges"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("node").or(Some(item)))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn total_count(value: Option<&Value>) -> Option<usize> {
    value.and_then(|value| {
        value
            .get("totalCount")
            .or_else(|| value.get("total_count"))
            .and_then(Value::as_u64)
            .and_then(|count| usize::try_from(count).ok())
    })
}

fn comments_from(value: Option<&Value>) -> Vec<GithubComment> {
    collection_nodes(value)
        .into_iter()
        .map(comment_from)
        .collect()
}

fn comment_from(value: &Value) -> GithubComment {
    GithubComment {
        author: actor_login(value.get("author")).or_else(|| value_string(value, "author")),
        body: value_string(value, "body").unwrap_or_default(),
        created_at: value_string(value, "createdAt").or_else(|| value_string(value, "created_at")),
        updated_at: value_string(value, "updatedAt").or_else(|| value_string(value, "updated_at")),
        minimized: value
            .get("isMinimized")
            .or_else(|| value.get("minimized"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        path: value_string(value, "path"),
        line: value_u64(value, "line").or_else(|| value_u64(value, "originalLine")),
    }
}

fn labels_from(value: Option<&Value>) -> Vec<String> {
    collection_nodes(value)
        .into_iter()
        .filter_map(|label| {
            value_string(label, "name").or_else(|| label.as_str().map(str::to_owned))
        })
        .collect()
}

fn actors_from(value: Option<&Value>) -> Vec<String> {
    collection_nodes(value)
        .into_iter()
        .filter_map(|actor| actor_login(Some(actor)).or_else(|| actor.as_str().map(str::to_owned)))
        .collect()
}

fn milestone_from(value: Option<&Value>) -> Option<String> {
    let value = value?;
    value_string(value, "title").or_else(|| value.as_str().map(str::to_owned))
}

fn reactions_from(value: Option<&Value>) -> Vec<GithubReaction> {
    collection_nodes(value)
        .into_iter()
        .filter_map(|reaction| {
            let count = reaction
                .get("users")
                .and_then(|users| users.as_u64().or_else(|| value_u64(users, "totalCount")))
                .or_else(|| value_u64(reaction, "count"))
                .or_else(|| value_u64(reaction, "totalCount"))
                .unwrap_or(0);
            let content =
                value_string(reaction, "content").or_else(|| value_string(reaction, "name"))?;
            (count > 0).then_some(GithubReaction { content, count })
        })
        .collect()
}

fn files_from(value: Option<&Value>) -> Vec<GithubPullRequestFile> {
    collection_nodes(value)
        .into_iter()
        .filter_map(|file| {
            let path = value_string(file, "path").or_else(|| value_string(file, "name"))?;
            Some(GithubPullRequestFile {
                path,
                additions: value_u64(file, "additions"),
                deletions: value_u64(file, "deletions"),
                status: value_string(file, "status"),
            })
        })
        .collect()
}

fn reviews_from(value: Option<&Value>) -> Vec<GithubReview> {
    collection_nodes(value)
        .into_iter()
        .map(|review| GithubReview {
            author: actor_login(review.get("author")).or_else(|| value_string(review, "author")),
            body: value_string(review, "body").unwrap_or_default(),
            state: value_string(review, "state"),
            submitted_at: value_string(review, "submittedAt")
                .or_else(|| value_string(review, "submitted_at")),
            comments: comments_from(review.get("comments")),
            comments_total_count: total_count(review.get("comments")),
        })
        .collect()
}

fn review_comment_sections_from(value: Option<&Value>) -> Vec<GithubReviewCommentSection> {
    collection_nodes(value)
        .into_iter()
        .map(|section| GithubReviewCommentSection {
            author: actor_login(section.get("author")).or_else(|| value_string(section, "author")),
            submitted_at: value_string(section, "submittedAt")
                .or_else(|| value_string(section, "submitted_at")),
            comments: comments_from(section.get("comments")),
            comments_total_count: total_count(section.get("comments")),
            minimized_comments_count: value_usize(section, "minimizedCommentsCount")
                .or_else(|| value_usize(section, "minimized_comments_count")),
        })
        .collect()
}

/// Normalize the selected state-changing event records from one or more pages
/// returned by `gh api --paginate --slurp`.
pub fn normalize_timeline_events(json: &Value) -> Vec<GithubTimelineEvent> {
    let mut values = Vec::new();
    collect_timeline_values(json, &mut values);
    values.into_iter().filter_map(timeline_event_from).collect()
}

fn collect_timeline_values<'a>(value: &'a Value, values: &mut Vec<&'a Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_timeline_values(item, values);
            }
        }
        Value::Object(object) if object.contains_key("event") => values.push(value),
        Value::Object(object) => {
            if let Some(events) = object.get("events") {
                collect_timeline_values(events, values);
            }
        }
        _ => {}
    }
}

fn timeline_event_from(value: &Value) -> Option<GithubTimelineEvent> {
    let event = value_string(value, "event")?;
    if !matches!(
        event.as_str(),
        "closed"
            | "reopened"
            | "merged"
            | "labeled"
            | "unlabeled"
            | "assigned"
            | "unassigned"
            | "milestoned"
            | "demilestoned"
            | "renamed"
            | "review_requested"
            | "ready_for_review"
            | "converted_to_draft"
    ) {
        return None;
    }
    let rename = value.get("rename");
    Some(GithubTimelineEvent {
        actor: actor_login(value.get("actor")),
        event,
        created_at: value_string(value, "created_at").or_else(|| value_string(value, "createdAt")),
        label: value
            .get("label")
            .and_then(|label| value_string(label, "name")),
        assignee: actor_login(value.get("assignee")),
        milestone: value
            .get("milestone")
            .and_then(|milestone| value_string(milestone, "title")),
        rename_from: rename.and_then(|rename| value_string(rename, "from")),
        rename_to: rename.and_then(|rename| value_string(rename, "to")),
        requested_reviewer: actor_login(value.get("requested_reviewer")),
        commit_id: value_string(value, "commit_id").or_else(|| value_string(value, "commitId")),
    })
}

fn actor_login(value: Option<&Value>) -> Option<String> {
    let value = value?;
    value_string(value, "login")
        .or_else(|| value_string(value, "name"))
        .or_else(|| value.as_str().map(str::to_owned))
}

fn value_string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

fn value_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key)?.as_u64()
}

fn value_usize(value: &Value, key: &str) -> Option<usize> {
    value_u64(value, key).and_then(|value| usize::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::github_read::resource::GithubResourceKind;

    #[test]
    fn normalizes_cli_json_without_human_output_parsing() {
        let resource = GithubResource {
            kind: GithubResourceKind::Issue,
            number: 7,
            repository: None,
            comment_selector: None,
        };
        let document = normalize_structured_document(
            &resource,
            &json!({
                "url": "https://github.com/CortexKit/aft/issues/7",
                "number": 7,
                "title": "Fixture issue",
                "state": "OPEN",
                "author": { "login": "octo" },
                "comments": { "totalCount": 2, "nodes": [
                    { "author": { "login": "reviewer" }, "body": "one", "createdAt": "2026-01-01T00:00:00Z" }
                ] },
                "reactionGroups": [{ "content": "THUMBS_UP", "users": 2 }]
            }),
        )
        .unwrap();

        assert_eq!(document.repository, "CortexKit/aft");
        assert_eq!(document.comments_total_count, Some(2));
        assert_eq!(document.reactions[0].count, 2);
    }

    #[test]
    fn timeline_fixture_keeps_selected_events_and_inline_review_locations() {
        let resource = GithubResource {
            kind: GithubResourceKind::PullRequest,
            number: 999,
            repository: Some("cortexkit/aft".to_string()),
            comment_selector: None,
        };
        let primary = serde_json::from_str(include_str!("fixtures/pr-999-timeline.json"))
            .expect("parse timeline PR fixture");
        let review_comments = serde_json::from_str(include_str!(
            "fixtures/pr-999-timeline-review-comments.json"
        ))
        .expect("parse timeline review comment fixture");
        let timeline = serde_json::from_str(include_str!("fixtures/pr-999-timeline-events.json"))
            .expect("parse timeline event fixture");

        let document = normalize_structured_document(&resource, &primary)
            .expect("normalize timeline PR fixture");
        let review_document = normalize_structured_document(&resource, &review_comments)
            .expect("normalize inline review fixture");
        let events = normalize_timeline_events(&timeline);

        assert_eq!(document.comments.len(), 2);
        assert_eq!(document.reviews.len(), 2);
        assert_eq!(
            review_document.review_comment_sections[0].comments[1]
                .path
                .as_deref(),
            Some("src/timeline.rs")
        );
        assert_eq!(
            review_document.review_comment_sections[0].comments[1].line,
            Some(24)
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.event.as_str())
                .collect::<Vec<_>>(),
            ["labeled", "closed", "reopened"]
        );
        assert_eq!(events[1].actor.as_deref(), Some("aft-alfonso[bot]"));
    }
}
