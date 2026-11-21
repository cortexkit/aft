//! Live, bounded PR diffs. These never enter the document fallback cache.

use serde_json::Value;

use super::cache::GithubReadSelector;
use super::fetch::{
    redact_gh_error, GhCommandError, GhCommandRunner, GithubFetchRequest, GithubReadError,
};
use crate::list_envelope::{ListEnvelope, Reason, Total, Unit};

pub const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHANGED_PATHS: usize = 100;
const MAX_PAGE_BYTES: usize = 50 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubDiff {
    pub header: String,
    pub body: String,
    pub cap_hit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubDiffPage {
    pub start_line: usize,
    pub end_line: usize,
    pub lines_read: usize,
    pub truncated: bool,
}

fn bounded_output(
    runner: &impl GhCommandRunner,
    request: &GithubFetchRequest,
    args: &[String],
) -> Result<(Vec<u8>, bool), GithubReadError> {
    let (output, capped) = runner
        .run_bounded(&request.working_directory, args, MAX_DIFF_BYTES)
        .map_err(|error| match error {
            GhCommandError::NotFound => GithubReadError::GithubCliMissing,
            GhCommandError::Other(message) => GithubReadError::FetchFailed(format!(
                "could not start GitHub CLI: {}",
                redact_gh_error(&message)
            )),
        })?;
    // A process stopped at the ceiling need not exit successfully. The caller
    // discloses the ceiling instead of misreporting the killed process as a gh error.
    if !output.success && !capped {
        let message = if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        };
        return Err(GithubReadError::FetchFailed(redact_gh_error(
            &String::from_utf8_lossy(message),
        )));
    }
    Ok((output.stdout, capped))
}

fn metadata(
    runner: &impl GhCommandRunner,
    request: &GithubFetchRequest,
) -> Result<Value, GithubReadError> {
    let mut args = vec![
        "pr".to_string(),
        "view".to_string(),
        request.resource.number.to_string(),
    ];
    if let Some(repo) = &request.resource.repository {
        args.extend(["--repo".to_string(), repo.clone()]);
    }
    args.extend([
        "--json".to_string(),
        "headRefOid,changedFiles,url".to_string(),
    ]);
    let (bytes, capped) = bounded_output(runner, request, &args)?;
    if capped {
        return Err(GithubReadError::FetchFailed(format!("PR diff metadata reached the {MAX_DIFF_BYTES}-byte fetch ceiling; changed paths are incomplete. No diff returned.")));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        GithubReadError::InvalidStructuredResponse(format!(
            "GitHub CLI returned invalid diff metadata: {error}"
        ))
    })
}

pub(super) fn fetch_diff(
    runner: &impl GhCommandRunner,
    request: &GithubFetchRequest,
) -> Result<GithubDiff, GithubReadError> {
    let data = metadata(runner, request)?;
    let sha = data["headRefOid"]
        .as_str()
        .filter(|sha| sha.len() >= 7 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| {
            GithubReadError::InvalidStructuredResponse(
                "GitHub diff metadata is missing the PR head SHA".to_string(),
            )
        })?;
    let url = data["url"]
        .as_str()
        .and_then(|url| url::Url::parse(url).ok())
        .filter(|url| url.host_str() == Some("github.com"))
        .ok_or_else(|| {
            GithubReadError::InvalidStructuredResponse(
                "GitHub diff metadata is missing the resolved repository URL".to_string(),
            )
        })?;
    let segments: Vec<_> = url.path().trim_matches('/').split('/').collect();
    if segments.len() != 4
        || segments[2] != "pull"
        || segments[3] != request.resource.number.to_string()
    {
        return Err(GithubReadError::InvalidStructuredResponse(
            "GitHub diff metadata returned an invalid PR URL".to_string(),
        ));
    }
    let repository = format!("{}/{}", segments[0], segments[1]);
    // pr view's GraphQL file connection can omit paths on a large PR. Use the
    // paginated REST file list, projecting away its duplicate patch payloads.
    let file_args = vec![
        "api".to_string(),
        format!(
            "repos/{repository}/pulls/{}/files?per_page=100",
            request.resource.number
        ),
        "--paginate".to_string(),
        "--jq".to_string(),
        ".[] | {path: .filename, previousPath: .previous_filename}".to_string(),
    ];
    let (file_bytes, file_cap) = bounded_output(runner, request, &file_args)?;
    if file_cap {
        return Err(GithubReadError::FetchFailed(format!(
            "PR changed paths reached the {MAX_DIFF_BYTES}-byte fetch ceiling; no diff returned."
        )));
    }
    // A PR whose changes were all reverted lists no files: `.[]` prints
    // nothing, which is an empty list rather than an invalid line.
    let mut lines: Vec<_> = if file_bytes.is_empty() {
        Vec::new()
    } else {
        file_bytes.split(|byte| *byte == b'\n').collect()
    };
    if file_bytes.last() == Some(&b'\n') {
        lines.pop();
    }
    let files = lines
        .into_iter()
        .map(|line| {
            serde_json::from_slice::<Value>(line).map_err(|error| {
                GithubReadError::InvalidStructuredResponse(format!(
                    "GitHub CLI returned invalid changed paths: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let paths = files
        .iter()
        .map(|file| {
            file["path"].as_str().ok_or_else(|| {
                GithubReadError::InvalidStructuredResponse(
                    "GitHub diff metadata contains an invalid changed path".to_string(),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if data["changedFiles"].as_u64() != Some(paths.len() as u64) {
        return Err(GithubReadError::FetchFailed(
            "GitHub returned an incomplete changed-path list; no diff returned".to_string(),
        ));
    }
    let path = request.resource.diff_path.as_deref().unwrap_or("");
    let mut header = format!(
        "PR {repository}#{} diff — head {}",
        request.resource.number,
        &sha[..7]
    );
    if !path.is_empty() {
        header.push_str(&format!(" — {path}"));
    }
    if !path.is_empty() && !paths.contains(&path) {
        let mut listed = Vec::new();
        let mut listed_bytes = 0;
        for path in paths.iter().take(MAX_CHANGED_PATHS) {
            let line = format!("- {path}");
            if listed_bytes + line.len() + 1 > MAX_PAGE_BYTES {
                break;
            }
            listed_bytes += line.len() + 1;
            listed.push(line);
        }
        let message = format!("{header}\nRefused: path is not in this PR. Match a changed path exactly (renames use the new path).\nChanged paths:\n{}", listed.join("\n"));
        return Err(GithubReadError::InvalidResource(with_trailer(
            &message,
            ListEnvelope::new(
                listed.len(),
                Total::Exact(paths.len()),
                Unit::Paths,
                if listed.len() < paths.len() {
                    vec![Reason::Cap]
                } else {
                    vec![]
                },
                &["path"],
            ),
        )));
    }
    let args = vec![
        "pr".to_string(),
        "diff".to_string(),
        request.resource.number.to_string(),
        "--repo".to_string(),
        repository,
    ];
    let (bytes, cap_hit) = bounded_output(runner, request, &args)?;
    // A push during the two live requests would otherwise label new patch text
    // with an old SHA. Refuse that race rather than inventing a stable snapshot.
    if metadata(runner, request)?["headRefOid"].as_str() != Some(sha) {
        return Err(GithubReadError::FetchFailed(format!(
            "{header}\nPR head moved while fetching the diff; use read again."
        )));
    }
    let raw = String::from_utf8_lossy(&bytes);
    let mut body = String::new();
    let mut old_path = files
        .iter()
        .find(|file| file["path"].as_str() == Some(path))
        .and_then(|file| file["previousPath"].as_str())
        .map(str::to_string);
    let mut matched = false;
    for block in diff_blocks(&raw) {
        let new_path = block_path(block);
        if path.is_empty() || new_path.as_deref() == Some(path) {
            matched = true;
            if !path.is_empty() {
                old_path = old_path.or_else(|| {
                    block
                        .lines()
                        .find_map(|line| line.strip_prefix("rename from ").map(unquote_path))
                });
            }
            if block
                .lines()
                .any(|line| line.starts_with("Binary files ") || line == "GIT binary patch")
            {
                body.push_str(&format!(
                    "{}: binary file changed\n",
                    new_path.as_deref().unwrap_or("unknown path")
                ));
            } else {
                body.push_str(block);
                if !block.ends_with('\n') {
                    body.push('\n');
                }
            }
        }
    }
    if let Some(old_path) = old_path {
        header.push_str(&format!(" (renamed from {old_path})"));
    }
    if !matched && !path.is_empty() {
        if cap_hit {
            body = "Requested file was not reached before the fetch ceiling.\n".to_string();
        } else {
            return Err(GithubReadError::FetchFailed(format!(
                "{header}\nChanged path has no patch in the GitHub diff response."
            )));
        }
    }
    Ok(GithubDiff {
        header,
        body,
        cap_hit,
    })
}

fn diff_blocks(text: &str) -> impl Iterator<Item = &str> {
    let starts = text
        .match_indices("diff --git ")
        .filter(|(i, _)| *i == 0 || text.as_bytes()[i - 1] == b'\n')
        .map(|(i, _)| i);
    starts
        .clone()
        .zip(starts.skip(1).chain(std::iter::once(text.len())))
        .map(|(start, end)| &text[start..end])
}

fn block_path(block: &str) -> Option<String> {
    // +++ is authoritative for non-deletions; renames without hunks have only
    // rename-to. Deleted and binary paths come from the git header instead.
    if let Some(path) = block
        .lines()
        .find_map(|line| line.strip_prefix("rename to "))
    {
        return Some(unquote_path(path));
    }
    if let Some(path) = block
        .lines()
        .find_map(|line| line.strip_prefix("+++ "))
        .filter(|path| *path != "/dev/null")
    {
        return unquote_path(path).strip_prefix("b/").map(str::to_string);
    }
    let header = block.lines().next()?.strip_prefix("diff --git ")?;
    let path = if header.starts_with('"') {
        header
            .split_once("\" \"")
            .map(|(_, path)| format!("\"{path}"))
    } else {
        header
            .rsplit_once(" b/")
            .map(|(_, path)| format!("b/{path}"))
    }?;
    unquote_path(&path).strip_prefix("b/").map(str::to_string)
}

fn unquote_path(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
    else {
        return path.to_string();
    };
    let mut bytes = Vec::new();
    let mut input = inner.bytes().peekable();
    while let Some(byte) = input.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let Some(escaped) = input.next() else {
            break;
        };
        if (b'0'..=b'7').contains(&escaped) {
            let mut value = (escaped - b'0') as u16;
            for _ in 0..2 {
                if let Some(next) = input.next_if(|next| (b'0'..=b'7').contains(next)) {
                    value = value * 8 + (next - b'0') as u16;
                }
            }
            bytes.push(value as u8);
        } else {
            bytes.push(match escaped {
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                other => other,
            });
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn with_trailer(text: &str, envelope: ListEnvelope) -> String {
    crate::ndjson_text::build_ndjson_text(
        text,
        &serde_json::json!({"diff_list_envelope": envelope}),
        Some("diff"),
        false,
    )
}

impl GithubDiff {
    pub(super) fn page(self, selector: GithubReadSelector) -> super::cache::GithubReadCompletion {
        let lines: Vec<_> = self.body.lines().collect();
        let (start, end) = match selector {
            GithubReadSelector::LineRange {
                start_line,
                end_line,
                limit,
            } => {
                let start = start_line.saturating_sub(1).min(lines.len());
                let end = end_line
                    .unwrap_or_else(|| start_line.saturating_add(limit).saturating_sub(1))
                    .min(lines.len())
                    .max(start);
                (start, end)
            }
            _ => (0, lines.len()),
        };
        let mut body = String::new();
        let mut shown = 0;
        for line in &lines[start..end] {
            if body.len().saturating_add(line.len() + 1) > MAX_PAGE_BYTES {
                break;
            }
            body.push_str(line);
            body.push('\n');
            shown += 1;
        }
        let truncated = self.cap_hit || start > 0 || start + shown < lines.len();
        let mut content = format!("{}\n{body}", self.header);
        if self.cap_hit {
            content.push_str(&format!("\nIncomplete diff: fetched {MAX_DIFF_BYTES} bytes, reaching the {MAX_DIFF_BYTES}-byte ceiling. The diff may be partial; no complete diff is available from this read.\n"));
        }
        content = with_trailer(
            &content,
            ListEnvelope::new(
                shown,
                if self.cap_hit {
                    Total::AtLeast(lines.len())
                } else {
                    Total::Exact(lines.len())
                },
                Unit::Lines,
                if truncated { vec![Reason::Cap] } else { vec![] },
                &["startLine", "endLine", "path"],
            ),
        );
        super::cache::GithubReadCompletion {
            content,
            total_lines: lines.len(),
            freshness: super::cache::GithubReadFreshness::Fetched,
            attachments: Vec::new(),
            document: None,
            diff_page: Some(GithubDiffPage {
                start_line: start + 1,
                end_line: start + shown,
                lines_read: shown,
                truncated,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fetch::{GhCliFetcher, GhCommandOutput, GithubFetcher};
    use super::super::resource::parse_resource;
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    struct FixtureRunner {
        metadata: Value,
        patch: Vec<u8>,
        calls: Mutex<Vec<Vec<String>>>,
        file_pages: Option<Vec<Vec<Value>>>,
        file_list_response: Option<Vec<u8>>,
        moved: bool,
        failure: bool,
    }

    impl FixtureRunner {
        fn recorded() -> Self {
            Self {
                metadata: serde_json::from_str(include_str!("fixtures/pr-42-diff-metadata.json"))
                    .unwrap(),
                patch: include_bytes!("fixtures/pr-42.diff").to_vec(),
                calls: Mutex::new(Vec::new()),
                file_pages: None,
                file_list_response: None,
                moved: false,
                failure: false,
            }
        }
    }

    impl GhCommandRunner for FixtureRunner {
        fn run(&self, _: &Path, _: &[String]) -> Result<GhCommandOutput, GhCommandError> {
            panic!("diff fetch must use the bounded subprocess seam")
        }
        fn run_bounded(
            &self,
            cwd: &Path,
            args: &[String],
            cap: usize,
        ) -> Result<(GhCommandOutput, bool), GhCommandError> {
            assert_eq!(cwd, Path::new("fixture-checkout"));
            assert_eq!(cap, MAX_DIFF_BYTES);
            let mut calls = self.calls.lock().unwrap();
            calls.push(args.to_vec());
            if args.iter().any(|arg| arg == "--slurp")
                && args.iter().any(|arg| arg == "--jq" || arg == "--template")
            {
                return Ok((
                    GhCommandOutput {
                        success: false,
                        stdout: vec![],
                        stderr:
                            b"Error: the --slurp option is not supported with --jq or --template"
                                .to_vec(),
                    },
                    false,
                ));
            }
            if self.failure {
                return Ok((
                    GhCommandOutput {
                        success: false,
                        stdout: vec![],
                        stderr: b"token: ghp_fixture_secret not found".to_vec(),
                    },
                    false,
                ));
            }
            let bytes = match args[1].as_str() {
                "view" => {
                    assert_eq!(
                        &args[args.len() - 2..],
                        ["--json", "headRefOid,changedFiles,url"]
                    );
                    let mut metadata = self.metadata.clone();
                    if self.moved && calls.len() == 4 {
                        metadata["headRefOid"] =
                            Value::String("abcdef0123456789abcdef0123456789abcdef01".to_string());
                    }
                    serde_json::to_vec(&metadata).unwrap()
                }
                "diff" => {
                    assert_eq!(args, ["pr", "diff", "42", "--repo", "owner/repo"]);
                    self.patch.clone()
                }
                "repos/owner/repo/pulls/42/files?per_page=100" => {
                    assert_eq!(
                        &args[2..],
                        [
                            "--paginate",
                            "--jq",
                            ".[] | {path: .filename, previousPath: .previous_filename}"
                        ]
                    );
                    if let Some(response) = &self.file_list_response {
                        response.clone()
                    } else {
                        let pages = self.file_pages.clone().unwrap_or_else(|| {
                            vec![self.metadata["files"].as_array().unwrap().clone()]
                        });
                        pages
                            .iter()
                            .flatten()
                            .map(|file| {
                                serde_json::json!({
                                    "path": file["path"],
                                    "previousPath": file["previousPath"]
                                })
                                .to_string()
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                            .into_bytes()
                    }
                }
                _ => panic!("unexpected gh arguments {args:?}"),
            };
            let capped = bytes.len() >= cap;
            Ok((
                GhCommandOutput {
                    success: !capped,
                    stdout: bytes.into_iter().take(cap).collect(),
                    stderr: vec![],
                },
                capped,
            ))
        }
    }

    fn fetch(runner: FixtureRunner, link: &str) -> Result<GithubDiff, GithubReadError> {
        GhCliFetcher::new(runner).fetch_diff(&GithubFetchRequest {
            resource: parse_resource(link).unwrap(),
            working_directory: "fixture-checkout".into(),
        })
    }

    #[test]
    fn multi_page_file_list_preserves_renamed_paths() {
        let mut runner = FixtureRunner::recorded();
        let files = runner.metadata["files"].as_array().unwrap().clone();
        runner.file_pages = Some(vec![files[..1].to_vec(), files[1..].to_vec()]);

        let diff = fetch(runner, "pr://42/diff/src/new.rs").unwrap();
        assert!(diff.header.contains("renamed from src/old.rs"));
        assert!(diff.body.contains("+new name"));
    }

    #[test]
    fn zero_file_pr_returns_an_empty_diff() {
        let mut runner = FixtureRunner::recorded();
        runner.metadata["changedFiles"] = serde_json::json!(0);
        runner.metadata["files"] = serde_json::json!([]);
        runner.file_list_response = Some(Vec::new());
        runner.patch = Vec::new();

        let diff = fetch(runner, "pr://42/diff").unwrap();
        let page = diff.page(GithubReadSelector::WholeDocument);
        assert!(page
            .content
            .starts_with("PR owner/repo#42 diff — head 0123456"));
    }

    #[test]
    fn empty_or_invalid_changed_path_lines_are_rejected() {
        for response in [b"\n".as_slice(), b"not json\n".as_slice()] {
            let mut runner = FixtureRunner::recorded();
            runner.file_list_response = Some(response.to_vec());

            assert!(matches!(
                fetch(runner, "pr://42/diff"),
                Err(GithubReadError::InvalidStructuredResponse(_))
            ));
        }
    }

    #[test]
    fn recorded_whole_diff_has_head_sha_and_binary_entry() {
        for link in ["pr://42/diff", "pr://owner/repo/42/diff"] {
            let diff = fetch(FixtureRunner::recorded(), link).unwrap();
            let page = diff.page(GithubReadSelector::WholeDocument);
            assert!(page
                .content
                .starts_with("PR owner/repo#42 diff — head 0123456\n"));
            assert!(page.content.contains("+new main\n"));
            assert!(page
                .content
                .contains("rename from src/old.rs\nrename to src/new.rs"));
            assert!(page
                .content
                .ends_with("assets/logo.png: binary file changed\n"));
            assert!(!page.content.contains("Binary files "));
        }
    }

    #[test]
    fn recorded_single_and_renamed_paths_match_exactly() {
        for (path, included, excluded) in [
            ("src/main.rs", "+new main", "+new name"),
            ("src/new.rs", "+new name", "+new main"),
        ] {
            let diff = fetch(FixtureRunner::recorded(), &format!("pr://42/diff/{path}")).unwrap();
            assert!(diff.header.contains(path));
            if path == "src/new.rs" {
                assert!(diff.header.contains("renamed from src/old.rs"));
            }
            assert!(diff.body.contains(included));
            assert!(!diff.body.contains(excluded));
            assert!(!diff.body.contains("binary file changed"));
        }
        let binary = fetch(FixtureRunner::recorded(), "pr://42/diff/assets/logo.png").unwrap();
        assert_eq!(binary.body, "assets/logo.png: binary file changed\n");
    }

    #[test]
    fn unknown_and_old_paths_are_refused_with_changed_paths() {
        for path in ["src/old.rs", "src/main", "SRC/main.rs", "missing.rs"] {
            let error = fetch(FixtureRunner::recorded(), &format!("pr://42/diff/{path}"))
                .unwrap_err()
                .to_string();
            assert!(error.starts_with("PR owner/repo#42 diff — head 0123456"));
            assert!(error.contains("Refused: path is not in this PR"));
            assert!(error.contains("- src/main.rs\n- src/new.rs\n- assets/logo.png"));
        }
    }

    #[test]
    fn unknown_path_preview_is_bounded_with_list_envelope() {
        let mut runner = FixtureRunner::recorded();
        runner.metadata["files"] = Value::Array(
            (0..105)
                .map(|i| serde_json::json!({"path": format!("file-{i}")}))
                .collect(),
        );
        runner.metadata["changedFiles"] = serde_json::json!(105);
        let error = fetch(runner, "pr://42/diff/missing")
            .unwrap_err()
            .to_string();
        assert!(error.contains("shown 100 of 105 paths (cap) · narrow: path"));
        assert!(!error.contains("- file-100"));
    }

    #[test]
    fn paging_across_file_boundary_repeats_header_and_discloses_cut() {
        let diff = fetch(FixtureRunner::recorded(), "pr://42/diff").unwrap();
        let page = diff.page(GithubReadSelector::LineRange {
            start_line: 7,
            end_line: Some(9),
            limit: 2000,
        });
        assert!(page.content.starts_with("PR owner/repo#42 diff — head 0123456\n+new main\ndiff --git a/src/old.rs b/src/new.rs\nsimilarity index 80%\n"));
        assert!(page.content.contains("shown 3 of 18 lines (cap)"));
        assert_eq!(
            page.diff_page.unwrap(),
            GithubDiffPage {
                start_line: 7,
                end_line: 9,
                lines_read: 3,
                truncated: true
            }
        );
    }

    #[test]
    fn byte_cap_is_disclosed_even_on_later_pages() {
        let mut runner = FixtureRunner::recorded();
        runner.patch.resize(MAX_DIFF_BYTES + 100, b'x');
        let diff = fetch(runner, "pr://42/diff").unwrap();
        assert!(diff.cap_hit);
        let page = diff.page(GithubReadSelector::LineRange {
            start_line: 7,
            end_line: Some(8),
            limit: 2,
        });
        assert!(page
            .content
            .contains("Incomplete diff: fetched 4194304 bytes, reaching the 4194304-byte ceiling"));
        assert!(page.content.contains("no complete diff is available"));
        assert!(page.diff_page.unwrap().truncated);
    }

    #[test]
    fn moved_head_and_cli_errors_refuse_without_leaking_credentials() {
        let mut runner = FixtureRunner::recorded();
        runner.moved = true;
        assert!(fetch(runner, "pr://42/diff")
            .unwrap_err()
            .to_string()
            .contains("head moved"));
        let mut runner = FixtureRunner::recorded();
        runner.failure = true;
        let error = fetch(runner, "pr://42/diff").unwrap_err().to_string();
        assert!(error.contains("[redacted]"));
        assert!(!error.contains("fixture_secret"));
    }

    #[test]
    fn quoted_git_paths_and_deleted_files_are_matched() {
        assert_eq!(
            block_path("diff --git a/old name b/new name\nrename to new name\n"),
            Some("new name".to_string())
        );
        assert_eq!(
            block_path(
                "diff --git \"a/caf\\303\\251.png\" \"b/caf\\303\\251.png\"\nBinary files differ\n"
            ),
            Some("café.png".to_string())
        );
        assert_eq!(
            block_path("diff --git a/gone b/gone\n--- a/gone\n+++ /dev/null\n"),
            Some("gone".to_string())
        );
    }
}
