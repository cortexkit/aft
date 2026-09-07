#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aft::db::github_read_cache::{GithubReadCacheEntry, GithubReadCacheKey};
use aft::github_read::{
    normalize_structured_document, parse_resource, render_document, DownloadedGithubImage,
    GithubDocument, GithubFetcher, GithubImageDownloader, GithubReadCacheStore, GithubReadClock,
    GithubReadCompletion, GithubReadEngine, GithubReadError, GithubReadFreshness,
    GithubReadSelector, GithubReadStart, GithubResourceKind,
};
use parking_lot::Mutex;
use serde_json::{json, Value};
use url::Url;

use super::helpers::AftProcess;

fn enabled_gh_read() -> aft::config::GhReadConfig {
    aft::config::GhReadConfig { enabled: true }
}

const FIXTURE_DIRECTORY: &str = "tests/fixtures/github_read_render_transport";
const FIXTURE_ISSUE_RESOURCE: &str = "issue://cortexkit/aft/73";
const FIXTURE_PR_RESOURCE: &str = "pr://cortexkit/aft/42";
const SHARED_SESSION: &str = "github-read-render-transport";

#[derive(Clone, Copy)]
enum TransportHarness {
    Ndjson,
    Subc,
    Mcp,
    OpenCode,
    Pi,
}

impl TransportHarness {
    const ALL: [Self; 5] = [
        Self::Ndjson,
        Self::Subc,
        Self::Mcp,
        Self::OpenCode,
        Self::Pi,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Ndjson => "standalone NDJSON",
            Self::Subc => "subc",
            Self::Mcp => "MCP-compatible",
            Self::OpenCode => "OpenCode",
            Self::Pi => "Pi",
        }
    }

    const fn configured_harness(self) -> &'static str {
        match self {
            Self::Ndjson | Self::Subc => "runner",
            Self::Mcp => "mcp:conformance",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
        }
    }

    const fn uses_tool_call(self) -> bool {
        !matches!(self, Self::Ndjson)
    }
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_DIRECTORY)
        .join(name)
}

fn fixture_json(name: &str) -> Value {
    serde_json::from_str(
        &fs::read_to_string(fixture_path(name))
            .unwrap_or_else(|error| panic!("read GitHub render fixture {name}: {error}")),
    )
    .unwrap_or_else(|error| panic!("parse GitHub render fixture {name}: {error}"))
}

fn fixture_markdown(name: &str) -> String {
    let fixture = fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|error| panic!("read pinned GitHub markdown {name}: {error}"));
    assert!(
        fixture.ends_with('\n'),
        "pinned GitHub markdown ends with a newline"
    );
    // The renderer emits one final blank line. Keep that byte explicit without a
    // whitespace-only final line in the checked-in Markdown fixture.
    format!("{fixture}\n")
}

fn fixture_document(resource: &str) -> GithubDocument {
    let resource = parse_resource(resource).expect("parse fixture resource");
    let document_name = match resource.kind {
        GithubResourceKind::Issue => "issue.json",
        GithubResourceKind::PullRequest => "pr.json",
    };
    let mut document = normalize_structured_document(&resource, &fixture_json(document_name))
        .expect("normalize primary structured GitHub fixture");
    if resource.kind == GithubResourceKind::PullRequest {
        let review_document =
            normalize_structured_document(&resource, &fixture_json("pr-review-comments.json"))
                .expect("normalize structured PR review-comment fixture");
        document.review_comment_sections = review_document.review_comment_sections;
    }
    document
}

fn assert_omp_shape_with_aft_metadata(rendered: &str, document: &GithubDocument) {
    let title = match document.kind {
        aft::github_read::GithubDocumentKind::Issue => "Issue",
        aft::github_read::GithubDocumentKind::PullRequest => "Pull request",
    };
    assert!(
        rendered.starts_with(&format!(
            "# {title} #{}: {}\n\n",
            document.number, document.title
        )),
        "the OMP-shaped title header must lead the canonical document"
    );

    let (metadata, body_and_sections) = rendered
        .split_once("\n## Body\n\n")
        .expect("OMP-shaped document has a body section after terse metadata");
    let permitted_aft_metadata = [
        "Repository:",
        "State:",
        "Author:",
        "Created:",
        "Updated:",
        "Labels:",
        "Assignees:",
        "Milestone:",
        "Reactions:",
    ];
    for line in metadata.lines().skip(2) {
        assert!(
            permitted_aft_metadata
                .iter()
                .any(|prefix| line.starts_with(prefix)),
            "metadata line {line:?} is not one of the explicitly permitted AFT additions"
        );
    }
    assert!(metadata.contains(&format!("Repository: {}", document.repository)));
    assert!(body_and_sections.contains(&document.body));

    if !document.comments.is_empty() || document.comments_total_count.unwrap_or(0) > 0 {
        assert!(
            body_and_sections.contains("## Comments\n\n"),
            "OMP-shaped issue discussion comments must have their own section"
        );
        assert!(
            body_and_sections.contains("### [1] @"),
            "OMP-shaped comments must expose ordinal author/date blocks"
        );
    }
    if document.kind == aft::github_read::GithubDocumentKind::PullRequest {
        for section in ["## Files\n\n", "## Reviews\n\n", "## Review comments\n\n"] {
            assert!(
                body_and_sections.contains(section),
                "OMP-shaped pull requests must include {section:?}"
            );
        }
        assert!(body_and_sections.contains("@inline-one ·"));
    }
}

#[test]
fn fixture_pinned_issue_and_pull_request_renders_follow_omp_shape() {
    for (resource, expected_name) in [
        (FIXTURE_ISSUE_RESOURCE, "issue.expected.md"),
        (FIXTURE_PR_RESOURCE, "pr.expected.md"),
    ] {
        let document = fixture_document(resource);
        let rendered = render_document(&document);
        assert_eq!(
            rendered,
            fixture_markdown(expected_name),
            "canonical render for {resource} changed from its fixture-pinned bytes"
        );
        assert_omp_shape_with_aft_metadata(&rendered, &document);
    }
}

fn write_fake_gh(bin_dir: &Path) {
    fs::create_dir_all(bin_dir).expect("create fixture gh directory");
    let script = bin_dir.join("gh");
    fs::write(
        &script,
        r#"#!/bin/sh
printf '%s %s\n' "$1" "$2" >> "$AFT_GH_READ_CALL_LOG"
case "$1:$2" in
  issue:view)
    cat "$AFT_GH_READ_FIXTURE_DIR/issue.json"
    ;;
  pr:view)
    cat "$AFT_GH_READ_FIXTURE_DIR/pr.json"
    ;;
   api:graphql)
     cat "$AFT_GH_READ_FIXTURE_DIR/pr-review-comments.json"
     ;;
   api:repos/*)
     printf '[]\n'
     ;;
   *)
    printf 'unexpected fixture gh command: %s %s\n' "$1" "$2" >&2
    exit 1
    ;;
esac
"#,
    )
    .expect("write fixture gh script");
    let mut permissions = fs::metadata(&script)
        .expect("stat fixture gh script")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("make fixture gh script executable");
}

fn spawn_fixture_aft(bin_dir: &Path, call_log: &Path) -> AftProcess {
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin_dir.to_path_buf()).chain(std::env::split_paths(&original_path)),
    )
    .expect("join fixture gh PATH");
    let fixture_dir = fixture_path("");
    AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("AFT_GH_READ_FIXTURE_DIR", fixture_dir.as_os_str()),
        ("AFT_GH_READ_CALL_LOG", call_log.as_os_str()),
    ])
}

fn configure_harness(aft: &mut AftProcess, project: &Path, storage: &Path, harness: &str) {
    // The transport suites exercise an ENABLED gh_read surface via a USER-tier
    // config (the only tier that can enable it); the gate's default-off
    // contract is covered by the dedicated disabled-read tests.
    let user_config = project.join("user-aft.jsonc");
    fs::write(&user_config, "{\"gh_read\": {\"enabled\": true}}")
        .expect("write gh_read user config");
    let response = aft.send(
        &json!({
            "id": format!("configure-github-render-{harness}"),
            "command": "configure",
            "harness": harness,
            "project_root": project,
            "storage_dir": storage,
            "cortexkit_user_config_path": user_config,
        })
        .to_string(),
    );
    assert_eq!(
        response["success"], true,
        "configure {harness} harness: {response:#}"
    );
}

fn read_request(
    harness: TransportHarness,
    id: &str,
    resource: &str,
    selected: Option<(usize, usize)>,
) -> Value {
    if harness.uses_tool_call() {
        let mut arguments = json!({ "path": resource })
            .as_object()
            .expect("tool-call arguments object")
            .clone();
        if let Some((start_line, end_line)) = selected {
            arguments.insert("offset".to_string(), json!(start_line));
            arguments.insert(
                "limit".to_string(),
                json!(end_line.saturating_sub(start_line).saturating_add(1)),
            );
        }
        json!({
            "id": id,
            "command": "tool_call",
            "session_id": SHARED_SESSION,
            "name": "read",
            "arguments": arguments,
        })
    } else {
        let mut request = json!({
            "id": id,
            "command": "read",
            "session_id": SHARED_SESSION,
            "file": resource,
        })
        .as_object()
        .expect("standalone request object")
        .clone();
        if let Some((start_line, end_line)) = selected {
            request.insert("start_line".to_string(), json!(start_line));
            request.insert("end_line".to_string(), json!(end_line));
        }
        Value::Object(request)
    }
}

fn read_through_harness(
    harness: TransportHarness,
    bin_dir: &Path,
    call_log: &Path,
    project: &Path,
    storage: &Path,
    resource: &str,
    selected: (usize, usize),
) -> Value {
    let mut aft = spawn_fixture_aft(bin_dir, call_log);
    configure_harness(&mut aft, project, storage, harness.configured_harness());

    let whole = aft.send(
        &read_request(
            harness,
            &format!("{}-whole", harness.label()),
            resource,
            None,
        )
        .to_string(),
    );
    assert_eq!(
        whole["success"],
        true,
        "{} whole-document read failed: {whole:#}",
        harness.label()
    );

    let selected = aft.send(
        &read_request(
            harness,
            &format!("{}-selected", harness.label()),
            resource,
            Some(selected),
        )
        .to_string(),
    );
    assert_eq!(
        selected["success"],
        true,
        "{} selected read failed: {selected:#}",
        harness.label()
    );
    assert!(aft.shutdown().success());
    selected
}

fn canonical_line_range(text: &str, start_line: usize, end_line: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let selected = lines[start_line.saturating_sub(1)..end_line].join("\n");
    format!("{selected}\n")
}

#[test]
fn fixture_selected_markdown_is_byte_identical_across_all_transport_harnesses() {
    let fixture_root = tempfile::tempdir().expect("create fixture gh root");
    let bin_dir = fixture_root.path().join("bin");
    let call_log = fixture_root.path().join("gh-calls.log");
    write_fake_gh(&bin_dir);
    fs::write(&call_log, "").expect("create fixture gh call log");

    let project = tempfile::tempdir().expect("create configured project");
    let storage = tempfile::tempdir().expect("create shared cache storage");
    let canonical = fixture_markdown("pr.expected.md");
    let selected_lines = (3, 27);
    let expected_selected = canonical_line_range(&canonical, selected_lines.0, selected_lines.1);
    let mut observed = BTreeSet::new();

    for harness in TransportHarness::ALL {
        let response = read_through_harness(
            harness,
            &bin_dir,
            &call_log,
            project.path(),
            storage.path(),
            FIXTURE_PR_RESOURCE,
            selected_lines,
        );
        let content = response["content"].as_str().unwrap_or_else(|| {
            panic!(
                "{} response lacks canonical content: {response:#}",
                harness.label()
            )
        });
        assert_eq!(
            content,
            expected_selected,
            "{} selected different bytes from the completed canonical PR render",
            harness.label()
        );
        observed.insert(content.as_bytes().to_vec());
    }

    assert_eq!(
        observed.len(),
        1,
        "standalone, subc, MCP-compatible, OpenCode, and Pi transports must agree byte-for-byte"
    );
    let expected_calls = TransportHarness::ALL
        .iter()
        .flat_map(|_| {
            [
                "pr view",
                "api graphql",
                "api repos/cortexkit/aft/issues/42/timeline?per_page=100",
                "pr view",
                "api graphql",
                "api repos/cortexkit/aft/issues/42/timeline?per_page=100",
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(
        fs::read_to_string(&call_log)
            .expect("read fixture gh call log")
            .lines()
            .collect::<Vec<_>>(),
        expected_calls,
        "every whole and selected transport read must fetch live"
    );
}

#[test]
fn start_line_and_offset_select_from_the_completed_canonical_render() {
    let fixture_root = tempfile::tempdir().expect("create selector fixture root");
    let bin_dir = fixture_root.path().join("bin");
    let call_log = fixture_root.path().join("gh-calls.log");
    write_fake_gh(&bin_dir);
    fs::write(&call_log, "").expect("create selector fixture gh call log");

    let project = tempfile::tempdir().expect("create selector project");
    let storage = tempfile::tempdir().expect("create selector storage");
    let canonical = fixture_markdown("issue.expected.md");
    let selected_lines = (3, 4);
    let expected_selected = canonical_line_range(&canonical, selected_lines.0, selected_lines.1);
    assert_eq!(
        expected_selected,
        "Repository: cortexkit/aft\nState: OPEN\n"
    );
    assert!(
        !fixture_json("issue.json")
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("Repository: cortexkit/aft"),
        "the selected header metadata exists only after the canonical render completes"
    );

    let standalone = read_through_harness(
        TransportHarness::Ndjson,
        &bin_dir,
        &call_log,
        project.path(),
        storage.path(),
        FIXTURE_ISSUE_RESOURCE,
        selected_lines,
    );
    let subc_offset = read_through_harness(
        TransportHarness::Subc,
        &bin_dir,
        &call_log,
        project.path(),
        storage.path(),
        FIXTURE_ISSUE_RESOURCE,
        selected_lines,
    );
    assert_eq!(standalone["content"], expected_selected);
    assert_eq!(
        subc_offset["content"], expected_selected,
        "subc offset/limit must select the same post-render metadata lines as standalone start_line/end_line"
    );
}

#[derive(Default)]
struct MemoryCache(Mutex<Option<GithubReadCacheEntry>>);

impl GithubReadCacheStore for MemoryCache {
    fn lookup(&self, _key: &GithubReadCacheKey) -> Result<Option<GithubReadCacheEntry>, String> {
        Ok(self.0.lock().clone())
    }

    fn upsert(
        &self,
        _key: &GithubReadCacheKey,
        canonical_text: &str,
        fetched_at_ms: i64,
    ) -> Result<(), String> {
        *self.0.lock() = Some(GithubReadCacheEntry {
            canonical_text: canonical_text.to_string(),
            fetched_at_ms,
            updated_at_ms: fetched_at_ms,
        });
        Ok(())
    }

    fn invalidate(
        &self,
        _kind: GithubResourceKind,
        _repository: &str,
        _number: u64,
        _authentication_identity: Option<&str>,
    ) -> Result<usize, String> {
        Ok(0)
    }
}

struct FixtureFetcher {
    document: GithubDocument,
    calls: AtomicUsize,
}

impl GithubFetcher for FixtureFetcher {
    fn fetch(
        &self,
        _request: &aft::github_read::GithubFetchRequest,
    ) -> Result<GithubDocument, GithubReadError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.document.clone())
    }
}

#[derive(Default)]
struct FixtureImageDownloader(AtomicUsize);

impl GithubImageDownloader for FixtureImageDownloader {
    fn download(
        &self,
        url: &Url,
        _maximum_bytes: usize,
    ) -> Result<Option<DownloadedGithubImage>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Some(DownloadedGithubImage {
            final_url: url.clone(),
            mime: "image/png".to_string(),
            bytes: vec![137, 80, 78, 71, 13, 10, 26, 10],
        }))
    }
}

struct FixedClock;

impl GithubReadClock for FixedClock {
    fn now_ms(&self) -> i64 {
        1_000
    }
}

fn complete_read(start: GithubReadStart) -> GithubReadCompletion {
    match start {
        GithubReadStart::Immediate(completion) => completion,
        GithubReadStart::Deferred(deferred) => {
            for _ in 0..1_000 {
                if let Some(result) = deferred.try_complete() {
                    return result.expect("fixture GitHub engine read succeeds");
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("fixture GitHub engine read did not complete")
        }
    }
}

#[test]
fn vision_and_text_only_fixture_requests_keep_selected_markdown_bytes_identical() {
    let fetcher = Arc::new(FixtureFetcher {
        document: fixture_document(FIXTURE_ISSUE_RESOURCE),
        calls: AtomicUsize::new(0),
    });
    let downloader = Arc::new(FixtureImageDownloader::default());
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        fetcher.clone(),
        downloader.clone(),
        Arc::new(FixedClock),
    );
    let selector = GithubReadSelector::WholeDocument;

    let text_only = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                FIXTURE_ISSUE_RESOURCE,
                "/fixture",
                "fixture-principal",
                None,
                selector.clone(),
            )
            .expect("start text-only fixture read"),
    );
    let vision = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                FIXTURE_ISSUE_RESOURCE,
                "/fixture",
                "fixture-principal",
                Some(true),
                selector,
            )
            .expect("start vision fixture read"),
    );

    assert_eq!(
        text_only.content, vision.content,
        "vision capability may add attachments but must not alter selected markdown bytes"
    );
    assert!(
        vision
            .content
            .contains("https://user-images.githubusercontent.com/1234/issue.png"),
        "the image URL remains intact in the selected markdown for vision callers"
    );
    assert!(
        text_only
            .content
            .contains("https://user-images.githubusercontent.com/1234/issue.png"),
        "the image URL remains intact in the selected markdown for text-only callers"
    );
    for content in [&text_only.content, &vision.content] {
        assert!(
            content.contains("https://github.com/user-attachments/files/73/comment.png"),
            "comment image URLs must remain intact for both capability modes"
        );
    }
    assert!(text_only.attachments.is_empty());
    assert_eq!(vision.attachments.len(), 2);
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);
    assert_eq!(downloader.0.load(Ordering::SeqCst), 2);
    assert_eq!(vision.freshness, GithubReadFreshness::Fetched);
}

fn live_read(harness: TransportHarness, project: &Path, storage: &Path, resource: &str) -> Value {
    let mut aft = AftProcess::spawn();
    configure_harness(&mut aft, project, storage, harness.configured_harness());
    let response = aft.send(
        &read_request(
            harness,
            &format!("github-read-live-{}", harness.label()),
            resource,
            None,
        )
        .to_string(),
    );
    assert!(aft.shutdown().success());
    response
}

#[test]
fn cortexkit_e2e_live_issue_matches_across_ndjson_subc_and_pi() {
    if std::env::var("AFT_GH_READ_LIVE").as_deref() != Ok("1") {
        eprintln!(
            "cortexkit-e2e GitHub read live-fire skipped; set AFT_GH_READ_LIVE=1, AFT_GH_READ_LIVE_RESOURCE=issue://OWNER/REPO/NUMBER, and AFT_GH_READ_LIVE_PROJECT_ROOT"
        );
        return;
    }

    let resource = std::env::var("AFT_GH_READ_LIVE_RESOURCE")
        .expect("AFT_GH_READ_LIVE_RESOURCE must name an explicit real issue URL");
    let explicit_path = resource
        .strip_prefix("issue://")
        .expect("AFT_GH_READ_LIVE_RESOURCE must use issue://");
    assert_eq!(
        explicit_path.split('/').count(),
        3,
        "AFT_GH_READ_LIVE_RESOURCE must be explicit so every transport shares the same cache key"
    );
    let project = std::env::var_os("AFT_GH_READ_LIVE_PROJECT_ROOT")
        .map(PathBuf::from)
        .expect("AFT_GH_READ_LIVE_PROJECT_ROOT must name the cortexkit-e2e project root");
    assert!(project.is_absolute(), "live project root must be absolute");

    let storage = tempfile::tempdir().expect("create shared live-fire cache");
    let ndjson = live_read(
        TransportHarness::Ndjson,
        &project,
        storage.path(),
        &resource,
    );
    let subc = live_read(TransportHarness::Subc, &project, storage.path(), &resource);
    let pi = live_read(TransportHarness::Pi, &project, storage.path(), &resource);
    for (label, response) in [("NDJSON", &ndjson), ("subc", &subc), ("Pi", &pi)] {
        assert_eq!(
            response["success"], true,
            "{label} cortexkit-e2e live-fire read failed: {response:#}"
        );
    }
    assert_eq!(ndjson["content"], subc["content"]);
    assert_eq!(ndjson["content"], pi["content"]);
    assert!(
        ndjson["content"]
            .as_str()
            .is_some_and(|content| !content.is_empty()),
        "a live GitHub read must return non-empty canonical markdown"
    );
}
