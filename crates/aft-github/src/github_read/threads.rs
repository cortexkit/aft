use serde_json::Value;

use super::fetch::{
    gh_pr_review_comments_args, GhCliFetcher, GhCommandRunner, GithubFetchRequest, GithubReadError,
};
use super::model::{GithubReviewThread, GithubReviewThreads};

// Bound remote work independently of how many comments the renderer displays.
const MAX_THREAD_PAGES: usize = 10;
const MAX_EXTRA_COMMENT_PAGES: usize = 10;
const THREAD_QUERY: &str = "query AftReadReviewThreads($owner: String!, $name: String!, $number: Int!, $cursor: String) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { reviewThreads(first: 100, after: $cursor) { totalCount pageInfo { hasNextPage endCursor } nodes { id isResolved isOutdated path line comments(first: 100) { totalCount pageInfo { hasNextPage endCursor } nodes { id } } } } } } }";
const COMMENT_QUERY: &str = "query AftReadThreadCommentIds($id: ID!, $cursor: String!) { node(id: $id) { ... on PullRequestReviewThread { comments(first: 100, after: $cursor) { totalCount pageInfo { hasNextPage endCursor } nodes { id } } } } }";

impl<R: GhCommandRunner> GhCliFetcher<R> {
    pub(super) fn fetch_review_threads(
        &self,
        request: &GithubFetchRequest,
        repository: &str,
    ) -> Result<GithubReviewThreads, GithubReadError> {
        let mut result = GithubReviewThreads::default();
        let mut cursor = None;
        let mut extra_comment_pages = 0;
        for page in 0..MAX_THREAD_PAGES {
            let mut args = gh_pr_review_comments_args(&request.resource, repository)?;
            args[3] = format!("query={THREAD_QUERY}");
            if let Some(cursor) = &cursor {
                args.extend(["-f".to_string(), format!("cursor={cursor}")]);
            }
            let json = self.structured_json(&request.working_directory, &args)?;
            let connection = json
                .pointer("/data/repository/pullRequest/reviewThreads")
                .ok_or_else(invalid_threads)?;
            let nodes = nodes(connection)?;
            let next = next_cursor(connection, cursor.as_deref())?;
            let total = total_count(connection)?;
            for node in nodes {
                let mut thread = GithubReviewThread {
                    id: string(node, "id")?,
                    is_resolved: node
                        .get("isResolved")
                        .and_then(Value::as_bool)
                        .ok_or_else(invalid_threads)?,
                    is_outdated: node
                        .get("isOutdated")
                        .and_then(Value::as_bool)
                        .ok_or_else(invalid_threads)?,
                    path: string(node, "path")?,
                    line: match node.get("line") {
                        Some(Value::Null) => None,
                        Some(value) => Some(value.as_u64().ok_or_else(invalid_threads)?),
                        None => return Err(invalid_threads()),
                    },
                    comment_ids: Vec::new(),
                };
                let comments = node.get("comments").ok_or_else(invalid_threads)?;
                append_ids(&mut thread.comment_ids, comments)?;
                let mut comment_total = total_count(comments)?;
                let mut comment_cursor = next_cursor(comments, None)?;
                while let Some(current) = comment_cursor.as_deref() {
                    if extra_comment_pages == MAX_EXTRA_COMMENT_PAGES {
                        break;
                    }
                    extra_comment_pages += 1;
                    let args = vec![
                        "api".to_string(),
                        "graphql".to_string(),
                        "-f".to_string(),
                        format!("query={COMMENT_QUERY}"),
                        "-f".to_string(),
                        format!("id={}", thread.id),
                        "-f".to_string(),
                        format!("cursor={current}"),
                    ];
                    let json = self.structured_json(&request.working_directory, &args)?;
                    let comments = json
                        .pointer("/data/node/comments")
                        .ok_or_else(invalid_threads)?;
                    append_ids(&mut thread.comment_ids, comments)?;
                    comment_total = total_count(comments)?;
                    comment_cursor = next_cursor(comments, Some(current))?;
                }
                if comment_cursor.is_some() {
                    result.omitted_comment_ids += comment_total
                        .saturating_sub(thread.comment_ids.len())
                        .max(1);
                } else if comment_total != thread.comment_ids.len() {
                    return Err(invalid_threads());
                }
                // Duplicate nodes must not inflate the number of threads collected.
                if result
                    .threads
                    .iter()
                    .any(|previous| previous.id == thread.id)
                {
                    return Err(invalid_threads());
                }
                result.threads.push(thread);
            }
            if next.is_none() {
                if total != result.threads.len() {
                    return Err(invalid_threads());
                }
                break;
            }
            if page + 1 == MAX_THREAD_PAGES {
                result.omitted_threads = total.saturating_sub(result.threads.len()).max(1);
            }
            cursor = next;
        }
        Ok(result)
    }
}

fn invalid_threads() -> GithubReadError {
    GithubReadError::InvalidStructuredResponse(
        "GitHub returned incomplete review thread state".to_string(),
    )
}

fn string(value: &Value, key: &str) -> Result<String, GithubReadError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(invalid_threads)
}

fn nodes(connection: &Value) -> Result<&Vec<Value>, GithubReadError> {
    connection
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(invalid_threads)
}

fn total_count(connection: &Value) -> Result<usize, GithubReadError> {
    connection
        .get("totalCount")
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(invalid_threads)
}

fn next_cursor(
    connection: &Value,
    current: Option<&str>,
) -> Result<Option<String>, GithubReadError> {
    let page = connection.get("pageInfo").ok_or_else(invalid_threads)?;
    match page.get("hasNextPage").and_then(Value::as_bool) {
        Some(false) => Ok(None),
        Some(true) => {
            let next = string(page, "endCursor")?;
            if Some(next.as_str()) == current {
                return Err(invalid_threads());
            }
            Ok(Some(next))
        }
        None => Err(invalid_threads()),
    }
}

fn append_ids(ids: &mut Vec<String>, connection: &Value) -> Result<(), GithubReadError> {
    for node in nodes(connection)? {
        let id = string(node, "id")?;
        if ids.contains(&id) {
            return Err(invalid_threads());
        }
        ids.push(id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::github_read::{
        parse_resource, render_document, GhCommandError, GhCommandOutput, GithubFetcher,
    };

    #[derive(Clone)]
    struct FixtureRunner {
        responses: Arc<Mutex<VecDeque<Value>>>,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl GhCommandRunner for FixtureRunner {
        fn run_bounded(
            &self,
            _: &std::path::Path,
            _: &[String],
            _: usize,
        ) -> Result<(GhCommandOutput, bool), GhCommandError> {
            panic!("thread fixtures do not fetch diffs")
        }

        fn run(
            &self,
            _: &std::path::Path,
            args: &[String],
        ) -> Result<GhCommandOutput, GhCommandError> {
            self.calls.lock().unwrap().push(args.to_vec());
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected GitHub call");
            Ok(GhCommandOutput {
                success: true,
                stdout: serde_json::to_vec(&response).unwrap(),
                stderr: Vec::new(),
            })
        }
    }

    fn fixture() -> Value {
        serde_json::from_str(include_str!("fixtures/pr-thread-state.json")).unwrap()
    }

    fn request() -> GithubFetchRequest {
        GithubFetchRequest {
            resource: parse_resource("pr://owner/repo/42").unwrap(),
            working_directory: std::path::PathBuf::from("."),
        }
    }

    fn runner(responses: Vec<Value>) -> FixtureRunner {
        FixtureRunner {
            responses: Arc::new(Mutex::new(responses.into())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn github_read_threads_paginate_and_join_open_resolved_outdated_comments_by_id() {
        let f = fixture();
        let runner = runner(vec![
            f["view"].clone(),
            f["reviews"].clone(),
            f["threadsPage1"].clone(),
            f["threadCommentsPage2"].clone(),
            f["threadsPage2"].clone(),
            json!([]),
        ]);
        let document = GhCliFetcher::new(runner.clone()).fetch(&request()).unwrap();
        let state = document.review_threads.as_ref().unwrap();
        assert_eq!(state.threads.len(), 3);
        assert_eq!(state.threads[0].comment_ids, ["C1", "C1_REPLY"]);
        assert_eq!(state.threads[1].path, "src/lib.rs");
        assert_eq!(state.threads[1].line, None);
        assert_eq!(state.omitted_threads, 0);
        assert_eq!(state.omitted_comment_ids, 0);
        let text = render_document(&document);
        assert!(text.contains("State: OPEN\nReview threads: 2 open, 1 resolved\n"));
        assert_eq!(text.matches("Review thread: PRRT_open — open\n").count(), 2);
        assert!(text.contains("Review thread: PRRT_resolved — resolved (outdated)"));
        assert!(text.contains("Review thread: PRRT_outdated_open — open (outdated)"));
        assert!(text.contains("[bot comment, compressed]\nCheck the guard."));
        assert!(!text.contains("Hidden bot detail."));
        let calls = runner.calls.lock().unwrap();
        assert!(calls[3].contains(&"id=PRRT_open".to_string()));
        assert!(calls[3].contains(&"cursor=comments-1".to_string()));
        assert!(calls[4].contains(&"cursor=threads-1".to_string()));
        assert!(runner.responses.lock().unwrap().is_empty());

        let mut selected = request().resource;
        selected.comment_selector =
            Some(crate::github_read::GithubCommentSelector::parse("2").unwrap());
        let text =
            super::super::render::render_document_for_resource(&document, &selected).unwrap();
        assert!(text.contains("Review thread: PRRT_resolved — resolved (outdated)"));
        assert!(text.contains("Guard fixed."));
    }

    #[test]
    fn github_read_thread_query_failure_preserves_comments_and_warns_once() {
        let f = fixture();
        for failure in [
            json!({"errors":[{"message":"fixture thread lookup denied"}]}),
            json!({"data":{"repository":{"pullRequest":null}}}),
        ] {
            let runner = runner(vec![
                f["view"].clone(),
                f["reviews"].clone(),
                failure,
                json!([]),
            ]);
            let document = GhCliFetcher::new(runner).fetch(&request()).unwrap();
            assert!(document.review_threads.is_none());
            let text = render_document(&document);
            assert_eq!(
                text.matches("Review thread state couldn't be read;")
                    .count(),
                1
            );
            assert!(!text.contains("Review threads: 0 open"));
            assert!(!text.contains("— resolved"));
            assert!(text.contains("Guard fixed."));
            assert!(text.contains("[bot comment, compressed]\nCheck the guard."));
        }
    }

    #[test]
    fn github_read_threads_missing_resolution_or_repeating_cursor_fail_closed() {
        let f = fixture();
        let mut page = f["threadsPage2"].clone();
        page["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("isResolved");
        let fetcher = GhCliFetcher::new(runner(vec![page]));
        assert!(fetcher
            .fetch_review_threads(&request(), "owner/repo")
            .is_err());
        let connection = json!({"pageInfo":{"hasNextPage":true,"endCursor":"same"}});
        assert!(next_cursor(&connection, Some("same")).is_err());
    }

    #[test]
    fn github_read_threads_bound_discloses_partial_counts_and_unmatched_comments() {
        let mut responses = Vec::new();
        for index in 0..MAX_THREAD_PAGES {
            responses.push(json!({"data":{"repository":{"pullRequest":{"reviewThreads":{
                "totalCount": MAX_THREAD_PAGES + 1,
                "pageInfo":{"hasNextPage":true,"endCursor":format!("page-{index}")},
                "nodes":[{"id":format!("T{index}"),"isResolved":true,"isOutdated":false,"path":"a.rs","line":1,
                    "comments":{"totalCount":0,"pageInfo":{"hasNextPage":false},"nodes":[]}}]
            }}}}}));
        }
        let runner = runner(responses);
        let state = GhCliFetcher::new(runner.clone())
            .fetch_review_threads(&request(), "owner/repo")
            .unwrap();
        assert_eq!(runner.calls.lock().unwrap().len(), MAX_THREAD_PAGES);
        assert_eq!(state.omitted_threads, 1);
        let mut document = super::super::normalize::normalize_structured_document(
            &request().resource,
            &fixture()["view"],
        )
        .unwrap();
        let reviews = super::super::normalize::normalize_structured_document(
            &request().resource,
            &fixture()["reviews"],
        )
        .unwrap();
        document.review_comment_sections = reviews.review_comment_sections;
        document.review_threads = Some(state);
        let text = render_document(&document);
        assert!(text.contains("Review threads: 0 open, 10 resolved (partial)"));
        assert!(text.contains("1 review threads omitted; their state is unknown."));
        assert!(text.contains("Review thread: unknown (state and ID unavailable)"));
    }

    #[test]
    fn github_read_thread_comment_id_budget_discloses_omissions() {
        let mut first = fixture()["threadsPage2"].clone();
        first["data"]["repository"]["pullRequest"]["reviewThreads"]["totalCount"] = json!(1);
        let comments = &mut first["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]
            ["comments"];
        *comments = json!({"totalCount":12,"pageInfo":{"hasNextPage":true,"endCursor":"c0"},"nodes":[{"id":"C0"}]});
        let mut responses = vec![first];
        for index in 1..=MAX_EXTRA_COMMENT_PAGES {
            responses.push(json!({"data":{"node":{"comments":{"totalCount":12,"pageInfo":{"hasNextPage":true,"endCursor":format!("c{index}")},"nodes":[{"id":format!("C{index}")}]}}}}));
        }
        let runner = runner(responses);
        let state = GhCliFetcher::new(runner.clone())
            .fetch_review_threads(&request(), "owner/repo")
            .unwrap();
        assert_eq!(
            runner.calls.lock().unwrap().len(),
            1 + MAX_EXTRA_COMMENT_PAGES
        );
        assert_eq!(state.omitted_comment_ids, 1);
        let document = crate::github_read::GithubDocument {
            review_threads: Some(state),
            ..Default::default()
        };
        assert!(super::super::thread_text::summary(&document).contains(
            "1 review-thread comment IDs omitted; some comments' thread state is unknown."
        ));
    }
}
