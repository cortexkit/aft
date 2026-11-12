use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use serde_json::Value;

use super::model::GithubDocument;
use super::normalize::{normalize_structured_document, normalize_timeline_events};
use super::resource::{GithubResource, GithubResourceKind};

const ISSUE_JSON_FIELDS: &str = "number,title,state,author,createdAt,updatedAt,labels,assignees,milestone,body,reactionGroups,comments,url";
const PR_JSON_FIELDS: &str = "number,title,state,author,createdAt,updatedAt,labels,assignees,milestone,body,reactionGroups,comments,files,reviews,baseRefName,headRefName,reviewDecision,url";
const PR_REVIEW_COMMENTS_QUERY: &str = "query AftReadPullRequestReviewComments($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { nameWithOwner pullRequest(number: $number) { number reviews(first: 100) { nodes { author { login } body state submittedAt comments(first: 100) { totalCount nodes { id author { login } body createdAt updatedAt isMinimized path line originalLine } } } } } } }";

/// Request context passed to a structured GitHub fetcher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubFetchRequest {
    pub resource: GithubResource,
    pub working_directory: PathBuf,
}

/// Fetches structured GitHub data. Implementations never return CLI display
/// text; the engine only accepts a normalized document from this interface.
pub trait GithubFetcher: Send + Sync {
    fn fetch(&self, request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError>;

    fn fetch_diff(
        &self,
        _request: &GithubFetchRequest,
    ) -> Result<super::diff::GithubDiff, GithubReadError> {
        Err(GithubReadError::FetchFailed(
            "GitHub diff fetching is unavailable".to_string(),
        ))
    }
}

/// The typed failures returned by the GitHub read engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GithubReadError {
    GithubReadDisabled,
    InvalidResource(String),
    InvalidCommentSelector(String),
    GithubCliMissing,
    FetchFailed(String),
    InvalidStructuredResponse(String),
    /// The live fetch was still running when the caller stopped waiting and
    /// no cached copy could stand in. The fetch continues in the background.
    Pending(String),
}

impl GithubReadError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::GithubReadDisabled => "gh_read_disabled",
            Self::InvalidResource(_) => "invalid_resource",
            Self::InvalidCommentSelector(_) => "invalid_comment_selector",
            Self::GithubCliMissing => "github_cli_missing",
            Self::FetchFailed(_) | Self::InvalidStructuredResponse(_) => "github_fetch_failed",
            Self::Pending(_) => "github_read_pending",
        }
    }

    pub fn invalid_resource(message: impl Into<String>) -> Self {
        Self::InvalidResource(message.into())
    }
}

impl fmt::Display for GithubReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GithubReadDisabled => formatter
                .write_str("GitHub reads are disabled; set github.read: true in aft.jsonc"),
            Self::InvalidResource(message)
            | Self::InvalidCommentSelector(message)
            | Self::FetchFailed(message)
            | Self::InvalidStructuredResponse(message)
            | Self::Pending(message) => formatter.write_str(message),
            Self::GithubCliMissing => formatter.write_str(
                "GitHub reads require the `gh` CLI. Install GitHub CLI and authenticate it with `gh auth login`.",
            ),
        }
    }
}

impl std::error::Error for GithubReadError {}

/// A subprocess result for the `gh` command seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GhCommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Error running `gh` before it produced a process result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GhCommandError {
    NotFound,
    Other(String),
}

/// Injectable `gh` runner. Fixture runners can assert the exact command and
/// request working directory without requiring a real GitHub installation.
pub trait GhCommandRunner: Send + Sync {
    fn run(
        &self,
        working_directory: &std::path::Path,
        args: &[String],
    ) -> Result<GhCommandOutput, GhCommandError>;

    /// Unlike `run`, this must bound subprocess reads before allocating output.
    fn run_bounded(
        &self,
        working_directory: &std::path::Path,
        args: &[String],
        byte_cap: usize,
    ) -> Result<(GhCommandOutput, bool), GhCommandError>;
}

/// Longest one `gh` command may run before it is killed. A `gh` stalled on a
/// connection would otherwise keep its read, and the thread that runs it,
/// alive indefinitely.
const GH_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Production runner that executes the bare `gh` command in the caller's
/// working directory so the CLI owns short-form repository resolution.
#[derive(Default)]
pub struct SystemGhCommandRunner;

impl GhCommandRunner for SystemGhCommandRunner {
    fn run(
        &self,
        working_directory: &std::path::Path,
        args: &[String],
    ) -> Result<GhCommandOutput, GhCommandError> {
        let mut command = Command::new("gh");
        command.args(args).current_dir(working_directory);
        run_command(command, None, GH_COMMAND_TIMEOUT).map(|(output, _)| output)
    }

    fn run_bounded(
        &self,
        working_directory: &std::path::Path,
        args: &[String],
        byte_cap: usize,
    ) -> Result<(GhCommandOutput, bool), GhCommandError> {
        let mut command = Command::new("gh");
        command.args(args).current_dir(working_directory);
        run_bounded_command(command, byte_cap)
    }
}

fn run_bounded_command(
    command: Command,
    byte_cap: usize,
) -> Result<(GhCommandOutput, bool), GhCommandError> {
    run_command(command, Some(byte_cap), GH_COMMAND_TIMEOUT)
}

enum PipeRead {
    Stdout(Result<Vec<u8>, GhCommandError>),
    Stderr(Result<Vec<u8>, GhCommandError>),
}

/// Run `command` with its output captured, `stdout` cut at `byte_cap` when
/// one is given, and the whole process tree killed if it is still running
/// after `timeout`. Returns the output and whether `stdout` was cut.
fn run_command(
    mut command: Command,
    byte_cap: Option<usize>,
    timeout: std::time::Duration,
) -> Result<(GhCommandOutput, bool), GhCommandError> {
    let deadline = std::time::Instant::now() + timeout;
    // No stdin: `gh` must fail rather than wait for an answer to a prompt.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::format::isolate_in_process_group(&mut command);
    let mut child = command.spawn().map_err(command_error)?;
    let stdout_pipe = child.stdout.take().expect("piped gh stdout");
    let stderr_pipe = child.stderr.take().expect("piped gh stderr");
    let (sender, receiver) = std::sync::mpsc::channel();
    let stderr_sender = sender.clone();
    // Drain stderr concurrently so a diagnostic cannot block stdout. Its
    // own iterator is bounded; credentials are redacted by the fetcher.
    std::thread::spawn(move || {
        let _ = stderr_sender.send(PipeRead::Stderr(read_capped(stderr_pipe, 64 * 1024)));
    });
    // A lookahead byte distinguishes exact-cap EOF from real overflow.
    let stdout_limit = byte_cap.map_or(usize::MAX, |cap| cap.saturating_add(1));
    std::thread::spawn(move || {
        let _ = sender.send(PipeRead::Stdout(read_capped(stdout_pipe, stdout_limit)));
    });

    let timed_out = |child: &mut std::process::Child| {
        crate::format::kill_process_tree(child);
        let _ = child.wait();
        // The reader threads are not joined: a descendant that escaped the
        // kill could hold a pipe open, and they end when it closes.
        GhCommandError::Other(format!(
            "GitHub CLI did not finish within {}s and was stopped",
            timeout.as_secs()
        ))
    };
    let mut stdout = None;
    let mut stderr = None;
    let mut cap_hit = false;
    while stdout.is_none() || stderr.is_none() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match receiver.recv_timeout(left) {
            Ok(PipeRead::Stdout(mut read)) => {
                if let (Some(cap), Ok(bytes)) = (byte_cap, read.as_mut()) {
                    if bytes.len() > cap {
                        bytes.pop();
                        cap_hit = true;
                    }
                }
                if cap_hit || read.is_err() {
                    crate::format::kill_process_tree(&mut child);
                }
                stdout = Some(read);
            }
            Ok(PipeRead::Stderr(read)) => stderr = Some(read),
            Err(_) => return Err(timed_out(&mut child)),
        }
    }
    // Both pipes are closed, so the process is exiting; its status can lag
    // the close by a moment.
    let mut pause = std::time::Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(command_error)? {
            break status;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(timed_out(&mut child));
        }
        std::thread::sleep(pause.min(left));
        pause = (pause * 2).min(std::time::Duration::from_millis(50));
    };
    Ok((
        GhCommandOutput {
            success: status.success(),
            stdout: stdout.expect("stdout read")?,
            stderr: stderr.expect("stderr read")?,
        },
        cap_hit,
    ))
}

fn command_error(error: std::io::Error) -> GhCommandError {
    match error.kind() {
        std::io::ErrorKind::NotFound => GhCommandError::NotFound,
        _ => GhCommandError::Other(error.to_string()),
    }
}

fn read_capped(reader: impl Read, byte_cap: usize) -> Result<Vec<u8>, GhCommandError> {
    let mut bytes = Vec::new();
    reader
        .take(byte_cap as u64)
        .read_to_end(&mut bytes)
        .map_err(command_error)?;
    Ok(bytes)
}

/// Structured `gh issue view` / `gh pr view` fetcher.
///
/// The only parser after the subprocess boundary is JSON normalization. In
/// particular, stderr is used only to construct an actionable redacted error,
/// never to infer fields or repository selection.
pub struct GhCliFetcher<R = SystemGhCommandRunner> {
    runner: R,
}

impl GhCliFetcher<SystemGhCommandRunner> {
    pub fn system() -> Self {
        Self {
            runner: SystemGhCommandRunner,
        }
    }
}

impl<R> GhCliFetcher<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }
}

impl<R: GhCommandRunner> GithubFetcher for GhCliFetcher<R> {
    fn fetch(&self, request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
        let json =
            self.structured_json(&request.working_directory, &gh_view_args(&request.resource))?;
        let mut document = normalize_document(&request.resource, &json)?;
        if request.resource.kind == GithubResourceKind::PullRequest {
            let review_json = self.structured_json(
                &request.working_directory,
                &gh_pr_review_comments_args(&request.resource, &document.repository)?,
            )?;
            let review_document = normalize_document(&request.resource, &review_json)?;
            document.review_comment_sections = review_document.review_comment_sections;
            // Thread lookup is independent: a failed lookup must not hide comments.
            document.review_threads = self
                .fetch_review_threads(request, &document.repository)
                .ok();
        }
        let timeline_json = self.structured_json(
            &request.working_directory,
            &gh_timeline_args(&request.resource, &document.repository)?,
        )?;
        document.timeline = normalize_timeline_events(&timeline_json);
        Ok(document)
    }

    fn fetch_diff(
        &self,
        request: &GithubFetchRequest,
    ) -> Result<super::diff::GithubDiff, GithubReadError> {
        super::diff::fetch_diff(&self.runner, request)
    }
}

impl<R: GhCommandRunner> GhCliFetcher<R> {
    pub(super) fn structured_json(
        &self,
        working_directory: &std::path::Path,
        args: &[String],
    ) -> Result<Value, GithubReadError> {
        let result = self
            .runner
            .run(working_directory, args)
            .map_err(|error| match error {
                GhCommandError::NotFound => GithubReadError::GithubCliMissing,
                GhCommandError::Other(message) => GithubReadError::FetchFailed(format!(
                    "could not start GitHub CLI: {}",
                    redact_gh_error(&message)
                )),
            })?;
        if !result.success {
            return Err(GithubReadError::FetchFailed(redact_gh_error(
                &best_gh_error(&result.stderr, &result.stdout),
            )));
        }
        let json: Value = serde_json::from_slice(&result.stdout).map_err(|error| {
            GithubReadError::InvalidStructuredResponse(format!(
                "GitHub CLI returned invalid structured JSON: {error}"
            ))
        })?;
        if json.get("success").and_then(Value::as_bool) == Some(false) {
            let underlying = json
                .get("error")
                .or_else(|| json.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("GitHub declined the resource request");
            return Err(GithubReadError::FetchFailed(redact_gh_error(underlying)));
        }
        if let Some(errors) = json.get("errors") {
            return Err(GithubReadError::FetchFailed(redact_gh_error(
                &errors.to_string(),
            )));
        }
        Ok(json)
    }
}

fn normalize_document(
    resource: &GithubResource,
    json: &Value,
) -> Result<GithubDocument, GithubReadError> {
    normalize_structured_document(resource, json).map_err(|error| {
        GithubReadError::InvalidStructuredResponse(format!(
            "GitHub returned an incomplete structured response: {}",
            redact_gh_error(&error.to_string())
        ))
    })
}

/// Build the exact structured-view invocation. Short forms intentionally omit
/// `-R`; explicit forms include it and are otherwise identical.
pub fn gh_view_args(resource: &GithubResource) -> Vec<String> {
    let fields = match resource.kind {
        GithubResourceKind::Issue => ISSUE_JSON_FIELDS,
        GithubResourceKind::PullRequest => PR_JSON_FIELDS,
    };
    let mut args = vec![
        resource.kind.command().to_string(),
        "view".to_string(),
        resource.number.to_string(),
    ];
    if let Some(repository) = &resource.repository {
        args.push("-R".to_string());
        args.push(repository.clone());
    }
    args.push("--json".to_string());
    args.push(fields.to_string());
    args
}

/// Build the paginated timeline request with the owner/repository resolved by
/// the initial resource lookup, expanding shorthand resource names first.
pub fn gh_timeline_args(
    resource: &GithubResource,
    resolved_repository: &str,
) -> Result<Vec<String>, GithubReadError> {
    let (owner, repository) = resolved_repository.split_once('/').ok_or_else(|| {
        GithubReadError::InvalidStructuredResponse(
            "GitHub structured response returned an invalid resolved repository".to_string(),
        )
    })?;
    if owner.is_empty() || repository.is_empty() || repository.contains('/') {
        return Err(GithubReadError::InvalidStructuredResponse(
            "GitHub structured response returned an invalid resolved repository".to_string(),
        ));
    }
    Ok(vec![
        "api".to_string(),
        format!(
            "repos/{owner}/{repository}/issues/{}/timeline?per_page=100",
            resource.number
        ),
        "--paginate".to_string(),
        "--slurp".to_string(),
    ])
}

pub fn gh_pr_review_comments_args(
    resource: &GithubResource,
    resolved_repository: &str,
) -> Result<Vec<String>, GithubReadError> {
    if resource.kind != GithubResourceKind::PullRequest {
        return Err(GithubReadError::InvalidStructuredResponse(
            "review-comment query requested for a non-pull-request resource".to_string(),
        ));
    }
    let (owner, repository) = resolved_repository.split_once('/').ok_or_else(|| {
        GithubReadError::InvalidStructuredResponse(
            "GitHub structured response returned an invalid resolved repository".to_string(),
        )
    })?;
    if owner.is_empty() || repository.is_empty() || repository.contains('/') {
        return Err(GithubReadError::InvalidStructuredResponse(
            "GitHub structured response returned an invalid resolved repository".to_string(),
        ));
    }
    let number = i32::try_from(resource.number).map_err(|_| {
        GithubReadError::InvalidStructuredResponse(
            "GitHub resource number exceeds the GraphQL integer range".to_string(),
        )
    })?;
    let args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("query={PR_REVIEW_COMMENTS_QUERY}"),
        "-F".to_string(),
        format!("owner={owner}"),
        "-F".to_string(),
        format!("name={repository}"),
        "-F".to_string(),
        format!("number={number}"),
    ];
    Ok(args)
}

fn best_gh_error(stderr: &[u8], stdout: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    "GitHub CLI failed without an error message".to_string()
}

static GITHUB_TOKEN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?:gh[pousr]_[A-Za-z0-9_]+|github_pat_[A-Za-z0-9_]+)")
        .expect("GitHub token redaction expression is valid")
});
static AUTHORIZATION_VALUE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)((?:authorization|token)\s*[:=]\s*)(?:(?:bearer|basic|token)\s+)?[^\s,;]+",
    )
    .expect("authorization redaction expression is valid")
});
static URL_USERINFO: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(//)[^/@\s]+@").expect("URL userinfo redaction expression is valid")
});

/// Redact ambient credentials while keeping GitHub's actionable authorization,
/// private-resource, and not-found diagnostics visible to the caller.
pub fn redact_gh_error(message: &str) -> String {
    // Remove URL credentials before matching token assignments; a userinfo
    // name such as x-access-token must not make that matcher eat the URL host.
    let with_userinfo = URL_USERINFO.replace_all(message, "${1}[redacted]@");
    let with_tokens = GITHUB_TOKEN.replace_all(&with_userinfo, "[redacted]");
    AUTHORIZATION_VALUE
        .replace_all(&with_tokens, "${1}[redacted]")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn exact_cap_stdout_is_not_truncated_but_overflow_is() {
        for (script, expected_cut) in [("printf abcd", false), ("printf abcde", true)] {
            let mut command = Command::new("/bin/sh");
            command.env_clear().args(["-c", script]);
            let (output, cut) = run_bounded_command(command, 4).unwrap();
            assert_eq!(output.stdout, b"abcd");
            assert_eq!(cut, expected_cut, "{script}");
            if !cut {
                assert!(output.success);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_command_still_running_at_its_timeout_is_killed() {
        // The background child keeps the output pipes open after the shell
        // itself is gone, as a hung helper of `gh` would.
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & sleep 30"]);
        let started = std::time::Instant::now();
        let error = run_command(command, Some(1024), std::time::Duration::from_millis(300))
            .expect_err("a hung command must be stopped");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(
            matches!(&error, GhCommandError::Other(message) if message.contains("did not finish")),
            "{error:?}"
        );
    }

    #[test]
    fn authorization_schemes_do_not_leak_credentials() {
        for message in [
            "Authorization: Bearer secret-bearer; permission denied",
            "authorization=Basic secret-basic, permission denied",
            "Authorization: token secret-token\npermission denied",
        ] {
            let redacted = redact_gh_error(message);
            assert!(!redacted.contains("secret-"), "{redacted}");
            assert!(redacted.contains("permission denied"), "{redacted}");
        }
    }

    #[test]
    fn url_userinfo_does_not_leak_credentials() {
        for username in ["username", "x-access-token"] {
            let redacted = redact_gh_error(&format!(
                "https://{username}:secret-userinfo@github.com/owner/repo: permission denied",
            ));
            assert!(!redacted.contains("secret-userinfo"), "{redacted}");
            assert!(
                redacted.contains("github.com/owner/repo: permission denied"),
                "{redacted}"
            );
        }
    }

    #[test]
    fn bounded_reader_stops_at_the_iterator_ceiling() {
        struct CountingReader(usize);
        impl Read for CountingReader {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                self.0 += output.len();
                output.fill(b'x');
                Ok(output.len())
            }
        }
        let mut reader = CountingReader(0);
        assert_eq!(read_capped(&mut reader, 37).unwrap().len(), 37);
        assert_eq!(
            reader.0, 37,
            "must not read then discard the rest of gh output"
        );
    }
    use crate::github_read::resource::{GithubResource, GithubResourceKind};

    #[derive(Default)]
    struct FixtureRunner {
        calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
        output: Mutex<Vec<Result<GhCommandOutput, GhCommandError>>>,
    }

    impl GhCommandRunner for FixtureRunner {
        fn run(
            &self,
            working_directory: &std::path::Path,
            args: &[String],
        ) -> Result<GhCommandOutput, GhCommandError> {
            self.calls
                .lock()
                .unwrap()
                .push((working_directory.to_path_buf(), args.to_vec()));
            self.output.lock().unwrap().remove(0)
        }

        fn run_bounded(
            &self,
            working_directory: &std::path::Path,
            args: &[String],
            byte_cap: usize,
        ) -> Result<(GhCommandOutput, bool), GhCommandError> {
            let mut output = self.run(working_directory, args)?;
            let capped = output.stdout.len() >= byte_cap;
            output.stdout.truncate(byte_cap);
            Ok((output, capped))
        }
    }

    #[test]
    fn explicit_and_short_forms_differ_only_by_repo_flag() {
        let short = GithubResource {
            kind: GithubResourceKind::Issue,
            number: 1,
            repository: None,
            comment_selector: None,
            diff_path: None,
        };
        let explicit = GithubResource {
            repository: Some("owner/repo".to_string()),
            ..short.clone()
        };
        let short_args = gh_view_args(&short);
        let explicit_args = gh_view_args(&explicit);
        assert!(!short_args.iter().any(|argument| argument == "-R"));
        assert!(explicit_args
            .windows(2)
            .any(|pair| pair == ["-R", "owner/repo"]));

        let short_pr = GithubResource {
            kind: GithubResourceKind::PullRequest,
            ..short.clone()
        };
        let explicit_pr = GithubResource {
            repository: Some("owner/repo".to_string()),
            ..short_pr.clone()
        };
        let short_review_args = gh_pr_review_comments_args(&short_pr, "owner/repo").unwrap();
        assert!(!short_review_args.iter().any(|argument| argument == "-R"));
        let explicit_review_args = gh_pr_review_comments_args(&explicit_pr, "owner/repo").unwrap();
        assert!(
            !explicit_review_args.iter().any(|argument| argument == "-R"),
            "the GraphQL request resolves owner and repository from its variables"
        );
        assert!(short_review_args
            .iter()
            .any(|argument| argument.starts_with("query=query AftReadPullRequestReviewComments")));
    }

    #[test]
    fn fetcher_uses_structured_json_and_redacts_failures() {
        let runner = FixtureRunner::default();
        *runner.output.lock().unwrap() = vec![
            Ok(GhCommandOutput {
                success: true,
                stdout: serde_json::to_vec(&json!({
                    "number": 1,
                    "title": "fixture",
                    "url": "https://github.com/owner/repo/issues/1"
                }))
                .unwrap(),
                stderr: Vec::new(),
            }),
            Ok(GhCommandOutput {
                success: true,
                stdout: b"[]".to_vec(),
                stderr: Vec::new(),
            }),
        ];
        let fetcher = GhCliFetcher::new(runner);
        let request = GithubFetchRequest {
            resource: GithubResource {
                kind: GithubResourceKind::Issue,
                number: 1,
                repository: None,
                comment_selector: None,
                diff_path: None,
            },
            working_directory: PathBuf::from("/fixture"),
        };
        let document = fetcher.fetch(&request).unwrap();
        assert_eq!(document.repository, "owner/repo");

        let redacted = redact_gh_error("HTTP 401 token=ghp_secret github_pat_secret");
        assert!(!redacted.contains("secret"));
        assert!(redacted.contains("HTTP 401"));
    }
}
