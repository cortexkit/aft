//! Structured, cached `issue://` and `pr://` read engine.
//!
//! The read-command integration owns transport routing and protocol response
//! adaptation. This module owns everything specific to GitHub resources: strict
//! URL parsing, structured `gh` fetches, normalization, canonical rendering,
//! durable-cache coordination, deferred network work, and vision-gated images.

mod attachments;
mod bot_compress;
mod cache;
mod fetch;
mod model;
mod normalize;
mod render;
mod resource;

pub use attachments::{
    discover_github_image_urls, download_github_image_attachments, is_allowed_github_image_url,
    DownloadedGithubImage, GithubImageAttachment, GithubImageDownloader,
    ReqwestGithubImageDownloader, MAX_GITHUB_IMAGE_ATTACHMENTS, MAX_GITHUB_IMAGE_ATTACHMENT_BYTES,
};
pub use cache::{
    apply_selector, sqlite_cache_store, GithubReadCacheStore, GithubReadClock,
    GithubReadCompletion, GithubReadDeferred, GithubReadEngine, GithubReadFreshness,
    GithubReadRequest, GithubReadSelector, GithubReadStart, GithubReadView,
    SqliteGithubReadCacheStore, SystemGithubReadClock,
};
pub use fetch::{
    gh_pr_review_comments_args, gh_timeline_args, gh_view_args, redact_gh_error, GhCliFetcher,
    GhCommandError, GhCommandOutput, GhCommandRunner, GithubFetchRequest, GithubFetcher,
    GithubReadError, SystemGhCommandRunner,
};
pub use model::{
    GithubComment, GithubDocument, GithubDocumentKind, GithubPullRequestFile, GithubReaction,
    GithubReview, GithubReviewCommentSection, GithubTimelineEvent,
};
pub use normalize::{normalize_structured_document, normalize_timeline_events, NormalizeError};
pub use render::{render_document, render_outline_for_resource, MAX_RENDERED_COMMENTS_PER_SECTION};
pub use resource::{
    parse_resource, GithubCommentSelector, GithubResource, GithubResourceKind,
    InvalidGithubResource,
};
