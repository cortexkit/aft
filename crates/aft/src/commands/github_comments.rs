use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::context::AppContext;
use crate::github_read::{
    discussion_ordinal_for_comment_url, parse_resource, GithubReadSelector, GithubResource,
    GithubResourceKind,
};
use crate::protocol::{RawRequest, Response};

const SAFETY_NOTICE: &str =
    "Comments cannot be undone with aft_safety; deleting a comment is a deliberate `gh` act.";

pub(crate) fn is_github_resource_path(path: &str) -> bool {
    path.starts_with("issue://") || path.starts_with("pr://")
}

pub(crate) fn handle_comment_write(
    req: &RawRequest,
    ctx: &AppContext,
    resource_spelling: &str,
    body: &str,
) -> Response {
    if let Err(response) = require_write_enabled(req, ctx) {
        return response;
    }
    let resource = match parse_base_resource(req, resource_spelling, "write") {
        Ok(resource) => resource,
        Err(response) => return response,
    };
    if crate::edit::wants_preview(&req.params) {
        return Response::success(
            &req.id,
            serde_json::json!({
                "resource": resource.base_spelling(),
                "preview_diff": body,
                "text": body,
            }),
        );
    }
    let working_directory = working_directory(ctx);
    let output = match run_governed_gh(
        ctx,
        &working_directory,
        &comment_create_args(&resource),
        body,
    ) {
        Ok(output) => output,
        Err(error) => {
            return Response::error(&req.id, "github_write_failed", error);
        }
    };
    if !output.status.success() {
        return gh_failure_response(req, output, "GitHub comment creation failed");
    }
    let comment_url = match command_url(&output.stdout) {
        Some(url) => url,
        None => {
            return Response::error(
                &req.id,
                "github_write_failed",
                "GitHub comment creation succeeded but returned no comment URL",
            );
        }
    };

    let completion = match reread_resource(req, ctx, &resource.base_spelling(), working_directory) {
        Ok(completion) => completion,
        Err(response) => return response,
    };
    let ordinal = completion
        .document
        .as_ref()
        .and_then(|document| discussion_ordinal_for_comment_url(document, &comment_url));
    let Some(ordinal) = ordinal else {
        return Response::error(
            &req.id,
            "github_write_failed",
            "The comment was created, but its read ordinal could not be determined from the live GitHub response",
        );
    };
    let text = format!("{comment_url}\nComment ordinal: {ordinal}\n{SAFETY_NOTICE}");
    Response::success(
        &req.id,
        serde_json::json!({
            "resource": resource.base_spelling(),
            "comment_url": comment_url,
            "ordinal": ordinal,
            "text": text,
        }),
    )
}

pub(crate) fn handle_comment_edit(
    req: &RawRequest,
    ctx: &AppContext,
    resource_spelling: &str,
    match_text: &str,
    replacement: &str,
) -> Response {
    if let Err(response) = require_write_enabled(req, ctx) {
        return response;
    }
    let resource = match parse_resource(resource_spelling) {
        Ok(resource) => resource,
        Err(error) => {
            return Response::error(&req.id, "invalid_resource", format!("edit: {error}"));
        }
    };
    let Some(selector) = resource.comment_selector.as_ref() else {
        return Response::error(
            &req.id,
            "invalid_request",
            "edit: GitHub comment paths must use issue://N/comments/K or pr://N/comments/K",
        );
    };
    let Some(ordinal) = selector.single_positive_ordinal() else {
        return Response::error(
            &req.id,
            "invalid_request",
            "edit: GitHub comments support exactly one positive comment ordinal",
        );
    };
    let base = resource.without_comment_selector();
    let working_directory = working_directory(ctx);
    let completion =
        match reread_resource(req, ctx, &base.base_spelling(), working_directory.clone()) {
            Ok(completion) => completion,
            Err(response) => return response,
        };
    let Some(document) = completion.document.as_ref() else {
        return Response::error(
            &req.id,
            "github_write_failed",
            "GitHub comment editing requires a successful live read",
        );
    };
    let comment = match crate::github_read::discussion_target_at_ordinal(document, ordinal) {
        Some(crate::github_read::GithubDiscussionTarget::Comment(comment)) => comment.clone(),
        Some(crate::github_read::GithubDiscussionTarget::ReviewThreadComment) => {
            return Response::error(
                &req.id,
                "invalid_request",
                "Pull-request review-thread comments are out of scope; edit a conversation comment instead",
            );
        }
        Some(crate::github_read::GithubDiscussionTarget::Other) => {
            return Response::error(
                &req.id,
                "invalid_request",
                "The selected discussion ordinal is not an editable conversation comment; review-thread comments are out of scope",
            );
        }
        None => {
            return Response::error(
                &req.id,
                "invalid_comment_selector",
                format!("Comment ordinal {ordinal} is outside the live discussion"),
            );
        }
    };
    let comment_url = match comment.url.as_deref() {
        Some(url) => url,
        None => {
            return Response::error(
                &req.id,
                "github_write_failed",
                "The selected GitHub comment has no stable URL for an id-addressed edit",
            );
        }
    };
    let comment_id = match issue_comment_id(comment_url) {
        Some(id) => id,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "Pull-request review-thread comments are out of scope; the selected URL is not an issue comment",
            );
        }
    };
    let (edited_body, replacements) = match apply_comment_match(
        req,
        resource_spelling,
        &comment.body,
        match_text,
        replacement,
    ) {
        Ok(result) => result,
        Err(response) => return response,
    };
    if crate::edit::wants_preview(&req.params) {
        return Response::success(
            &req.id,
            serde_json::json!({
                "resource": resource_spelling,
                "ordinal": ordinal,
                "replacements": replacements,
                "preview_diff": edited_body,
                "text": edited_body,
            }),
        );
    }
    let patch_body = serde_json::json!({ "body": edited_body }).to_string();
    let args = vec![
        "api".to_string(),
        "--method".to_string(),
        "PATCH".to_string(),
        format!("repos/{}/issues/comments/{comment_id}", document.repository),
        "--input".to_string(),
        "-".to_string(),
    ];
    let output = match run_governed_gh(ctx, &working_directory, &args, &patch_body) {
        Ok(output) => output,
        Err(error) => return Response::error(&req.id, "github_write_failed", error),
    };
    if !output.status.success() {
        return gh_failure_response(req, output, "GitHub comment edit failed");
    }

    Response::success(
        &req.id,
        serde_json::json!({
            "resource": resource_spelling,
            "comment_url": comment_url,
            "ordinal": ordinal,
            "replacements": replacements,
            "text": format!("{comment_url}\nComment ordinal: {ordinal}\n{SAFETY_NOTICE}"),
        }),
    )
}

fn apply_comment_match(
    req: &RawRequest,
    resource: &str,
    source: &str,
    match_text: &str,
    replacement: &str,
) -> Result<(String, usize), Response> {
    let replace_all = req
        .params
        .get("replace_all")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let raw_occurrence = req.params.get("occurrence");
    if replace_all && raw_occurrence.is_some() {
        return Err(Response::error(
            &req.id,
            "invalid_request",
            "edit_match: 'replaceAll' and 'occurrence' are mutually exclusive",
        ));
    }
    let occurrence = match raw_occurrence {
        None => None,
        Some(value) => match value.as_u64() {
            Some(0) | None => {
                return Err(Response::error(
                    &req.id,
                    "invalid_request",
                    "edit_match: 'occurrence' must be a positive integer (1-based)",
                ));
            }
            Some(value) if value - 1 <= usize::MAX as u64 => Some((value - 1) as usize),
            Some(_) => {
                return Err(Response::error(
                    &req.id,
                    "invalid_request",
                    "edit_match: 'occurrence' exceeds the supported range",
                ));
            }
        },
    };
    let matches = crate::fuzzy_match::find_all_fuzzy(source, match_text);
    if matches.is_empty() {
        return Err(Response::error(
            &req.id,
            "match_not_found",
            format!(
                "edit_match: '{}' not found in {}{}",
                match_text,
                resource,
                crate::fuzzy_match::render_nearest_miss_detail(source, match_text)
            ),
        ));
    }
    if !replace_all {
        if let Some(index) = occurrence {
            if index >= matches.len() {
                return Err(Response::error(
                    &req.id,
                    "invalid_request",
                    format!(
                        "edit_match: occurrence {} out of range, comment has {} occurrence(s)",
                        index + 1,
                        matches.len()
                    ),
                ));
            }
        }
    }
    if matches.len() > 1 && occurrence.is_none() && !replace_all {
        return Err(Response::error(
            &req.id,
            "ambiguous_match",
            format!(
                "Found {} matches in the comment. Use 'occurrence' (1-based) or 'replaceAll: true'.",
                matches.len()
            ),
        ));
    }

    if replace_all {
        for pair in matches.windows(2) {
            if pair[0].byte_start + pair[0].byte_len > pair[1].byte_start {
                return Err(Response::error(
                    &req.id,
                    "overlapping_edits",
                    "edit: replace_all matches overlap; use a more specific match",
                ));
            }
        }
        let updated = super::edit_match::apply_sorted_non_overlapping_fuzzy_matches(
            source,
            &matches,
            replacement,
        )
        .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))?;
        Ok((updated, matches.len()))
    } else {
        let matched = &matches[occurrence.unwrap_or(0)];
        let mut effective = String::with_capacity(replacement.len().saturating_add(1));
        super::edit_match::push_fuzzy_replacement(&mut effective, source, matched, replacement);
        let updated = crate::edit::replace_byte_range(
            source,
            matched.byte_start,
            matched.byte_start + matched.byte_len,
            &effective,
        )
        .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))?;
        Ok((updated, 1))
    }
}

fn issue_comment_id(comment_url: &str) -> Option<u64> {
    comment_url
        .split_once("#issuecomment-")
        .and_then(|(_, id)| id.parse().ok())
}

pub(crate) fn require_write_enabled(req: &RawRequest, ctx: &AppContext) -> Result<(), Response> {
    if ctx.request_force_restrict(&req.id) {
        return Err(Response::error(
            &req.id,
            "github_write_disabled",
            "github.write is not enabled on untrusted or restricted binds",
        ));
    }
    if !ctx.config().github.write {
        return Err(Response::error(
            &req.id,
            "github_write_disabled",
            "GitHub comment writes are not enabled; set github.write: true in user config",
        ));
    }
    Ok(())
}

pub(crate) fn parse_base_resource(
    req: &RawRequest,
    spelling: &str,
    operation: &str,
) -> Result<GithubResource, Response> {
    let resource = parse_resource(spelling).map_err(|error| {
        Response::error(&req.id, "invalid_resource", format!("{operation}: {error}"))
    })?;
    if resource.comment_selector.is_some() {
        return Err(Response::error(
            &req.id,
            "invalid_request",
            format!("{operation}: use a base issue:// or pr:// resource without /comments/..."),
        ));
    }
    Ok(resource)
}

pub(crate) fn working_directory(ctx: &AppContext) -> PathBuf {
    ctx.config()
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()))
}

pub(crate) fn run_governed_gh(
    ctx: &AppContext,
    working_directory: &std::path::Path,
    args: &[String],
    body: &str,
) -> Result<std::process::Output, String> {
    let config = ctx.config();
    let mut environment: HashMap<String, String> = std::env::vars().collect();
    crate::agent_child_env::inject(&config, &ctx.storage_dir(), &mut environment)
        .map_err(|error| format!("could not prepare the governed gh shim: {error}"))?;

    let mut command = Command::new("gh");
    command
        .args(args)
        .current_dir(working_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::agent_child_env::apply_to_command(&mut command, &environment);
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start the governed gh shim: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "could not open stdin for the governed gh shim".to_string())?
        .write_all(body.as_bytes())
        .map_err(|error| {
            format!("could not send the comment body to the governed gh shim: {error}")
        })?;
    child
        .wait_with_output()
        .map_err(|error| format!("could not wait for the governed gh shim: {error}"))
}

pub(crate) fn gh_failure_response(
    req: &RawRequest,
    output: std::process::Output,
    fallback: &str,
) -> Response {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if output.status.code() == Some(crate::gh_shim::REFUSAL_EXIT_STATUS) {
        return Response::error(
            &req.id,
            "gh_shim_refused",
            if message.is_empty() {
                fallback
            } else {
                message
            },
        );
    }
    Response::error(
        &req.id,
        "github_write_failed",
        if message.is_empty() {
            fallback.to_string()
        } else {
            crate::github_read::redact_gh_error(message)
        },
    )
}

pub(crate) fn reread_resource(
    req: &RawRequest,
    ctx: &AppContext,
    spelling: &str,
    working_directory: PathBuf,
) -> Result<crate::github_read::GithubReadCompletion, Response> {
    let gh_read = ctx.config().gh_read.clone();
    let start = super::read::github_read_engine(ctx)
        .start_resource(
            &gh_read,
            spelling,
            working_directory,
            format!("session:{}", req.session()),
            None,
            GithubReadSelector::WholeDocument,
        )
        .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))?;
    super::read::wait_for_github_read(start)
        .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))
}

fn comment_create_args(resource: &GithubResource) -> Vec<String> {
    let mut args = vec![
        match resource.kind {
            GithubResourceKind::Issue => "issue",
            GithubResourceKind::PullRequest => "pr",
        }
        .to_string(),
        "comment".to_string(),
        resource.number.to_string(),
    ];
    if let Some(repository) = &resource.repository {
        args.extend(["-R".to_string(), repository.clone()]);
    }
    args.extend(["--body-file".to_string(), "-".to_string()]);
    args
}

fn command_url(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("https://") || line.starts_with("http://"))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, GithubConfig};
    use crate::context::default_language_provider_factory;
    use crate::harness::Harness;
    use serde_json::json;

    #[test]
    fn pull_request_comment_args_use_the_same_body_file_contract() {
        let resource = parse_resource("pr://owner/repo/12").expect("parse pull request");
        assert_eq!(
            comment_create_args(&resource),
            [
                "pr",
                "comment",
                "12",
                "-R",
                "owner/repo",
                "--body-file",
                "-"
            ]
        );
    }

    #[test]
    fn untrusted_bind_refuses_before_parsing_or_spawning_gh() {
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                github: GithubConfig {
                    write: true,
                    ..GithubConfig::default()
                },
                ..Config::default()
            },
        );
        ctx.set_harness(Harness::Mcp {
            client: "github-write-test".to_string(),
        });
        let request = RawRequest {
            id: "untrusted-github-write".to_string(),
            command: "write".to_string(),
            lsp_hints: None,
            session_id: Some("test-session".to_string()),
            params: json!({
                "file": "issue://7",
                "content": "must not publish",
            }),
        };

        let response = ctx.with_force_restrict(&request.id, || {
            handle_comment_write(&request, &ctx, "issue://7", "must not publish")
        });
        assert!(!response.success);
        assert_eq!(response.data["code"], "github_write_disabled");
        assert!(response.data["message"]
            .as_str()
            .unwrap()
            .contains("github.write"));
    }
}
