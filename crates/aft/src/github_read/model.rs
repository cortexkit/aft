use serde::{Deserialize, Serialize};

use super::resource::GithubResourceKind;

/// Structured GitHub data consumed by the canonical Rust renderer.
///
/// Fetchers normalize their provider-specific JSON into this model. Keeping the
/// renderer independent of CLI field spelling prevents accidental parsing of
/// human-oriented `gh` output and gives fixture tests a stable seam.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GithubDocument {
    pub repository: String,
    pub kind: GithubDocumentKind,
    pub number: u64,
    pub title: String,
    pub state: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub milestone: Option<String>,
    pub body: String,
    pub reactions: Vec<GithubReaction>,
    pub comments: Vec<GithubComment>,
    /// Total issue-discussion comments before the renderer applies its cap.
    pub comments_total_count: Option<usize>,
    /// Server-supplied count when it includes minimized comments outside the
    /// local page. The renderer also counts minimized comments it received.
    pub minimized_comments_count: Option<usize>,
    pub files: Vec<GithubPullRequestFile>,
    pub reviews: Vec<GithubReview>,
    pub review_comment_sections: Vec<GithubReviewCommentSection>,
    pub base_ref_name: Option<String>,
    pub head_ref_name: Option<String>,
    pub review_decision: Option<String>,
    /// Timeline events selected from GitHub's issue timeline endpoint.
    pub timeline: Vec<GithubTimelineEvent>,
}

impl GithubDocument {
    pub fn resource_kind(&self) -> GithubResourceKind {
        match self.kind {
            GithubDocumentKind::Issue => GithubResourceKind::Issue,
            GithubDocumentKind::PullRequest => GithubResourceKind::PullRequest,
        }
    }
}

/// Resource-kind mirror included in structured fixtures and normalized data.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubDocumentKind {
    #[default]
    Issue,
    PullRequest,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GithubReaction {
    pub content: String,
    pub count: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GithubComment {
    pub author: Option<String>,
    pub body: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub minimized: bool,
    pub path: Option<String>,
    pub line: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GithubPullRequestFile {
    pub path: String,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    pub status: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GithubReview {
    pub author: Option<String>,
    pub body: String,
    pub state: Option<String>,
    pub submitted_at: Option<String>,
    pub comments: Vec<GithubComment>,
    pub comments_total_count: Option<usize>,
}

/// One review's inline-comment section. Each section is capped independently,
/// so an active review cannot crowd comments out of a different review.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GithubReviewCommentSection {
    pub author: Option<String>,
    pub submitted_at: Option<String>,
    pub comments: Vec<GithubComment>,
    pub comments_total_count: Option<usize>,
    pub minimized_comments_count: Option<usize>,
}

/// A timeline state-change event normalized from GitHub's issue timeline API.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GithubTimelineEvent {
    pub actor: Option<String>,
    pub event: String,
    pub created_at: Option<String>,
    pub label: Option<String>,
    pub assignee: Option<String>,
    pub milestone: Option<String>,
    pub rename_from: Option<String>,
    pub rename_to: Option<String>,
    pub requested_reviewer: Option<String>,
    pub commit_id: Option<String>,
}
