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
