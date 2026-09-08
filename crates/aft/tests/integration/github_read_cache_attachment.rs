#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aft::github_read::{
    download_github_image_attachments, render_document, sqlite_cache_store, DownloadedGithubImage,
    GithubDocument, GithubDocumentKind, GithubFetchRequest, GithubFetcher, GithubImageDownloader,
    GithubReadClock, GithubReadCompletion, GithubReadEngine, GithubReadError, GithubReadFreshness,
    GithubReadSelector, GithubReadStart, GithubResourceKind, MAX_GITHUB_IMAGE_ATTACHMENTS,
    MAX_GITHUB_IMAGE_ATTACHMENT_BYTES,
};
use parking_lot::Mutex;
use url::Url;

fn enabled_gh_read() -> aft::config::GhReadConfig {
    aft::config::GhReadConfig { enabled: true }
}

const FIXTURE_DIRECTORY: &str = "tests/fixtures/github_read_cache_attachment";
const RESOURCE: &str = "issue://owner/repo/7";
const PNG_SIGNATURE: &[u8] = &[137, 80, 78, 71, 13, 10, 26, 10];

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_DIRECTORY)
        .join(name)
}

fn bug_report_fixture() -> GithubDocument {
    serde_json::from_str(
        &fs::read_to_string(fixture_path("bug_report.json"))
            .expect("read GitHub bug-report fixture"),
    )
    .expect("parse GitHub bug-report fixture")
}

struct FixtureClock(AtomicI64);

impl FixtureClock {
    fn new(now_ms: i64) -> Self {
        Self(AtomicI64::new(now_ms))
    }
}

impl GithubReadClock for FixtureClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct ProbeFetcher {
    calls: AtomicUsize,
}

impl ProbeFetcher {
    fn ordinary() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl GithubFetcher for ProbeFetcher {
    fn fetch(&self, request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let repository = request.resource.repository.clone().unwrap_or_else(|| {
            let context = request
                .working_directory
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unresolved");
            format!("owner/{context}")
        });
        let kind = match request.resource.kind {
            GithubResourceKind::Issue => GithubDocumentKind::Issue,
            GithubResourceKind::PullRequest => GithubDocumentKind::PullRequest,
        };
        Ok(GithubDocument {
            repository,
            kind,
            number: request.resource.number,
            title: format!("network revision {call}"),
            state: "OPEN".to_string(),
            body: format!("network body revision {call}"),
            ..GithubDocument::default()
        })
    }
}

struct FailingAfterFirstFetcher(AtomicUsize);

impl GithubFetcher for FailingAfterFirstFetcher {
    fn fetch(&self, request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
        let call = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        if call > 1 {
            return Err(GithubReadError::FetchFailed(
                "fixture live fetch failed".to_string(),
            ));
        }
        Ok(GithubDocument {
            repository: request
                .resource
                .repository
                .clone()
                .unwrap_or_else(|| "owner/repo".to_string()),
            kind: GithubDocumentKind::Issue,
            number: request.resource.number,
            title: "fallback seed".to_string(),
            state: "OPEN".to_string(),
            body: "cached fallback body".to_string(),
            ..GithubDocument::default()
        })
    }
}

struct StaticFixtureFetcher {
    document: GithubDocument,
    calls: AtomicUsize,
}

impl StaticFixtureFetcher {
    fn new(document: GithubDocument) -> Self {
        Self {
            document,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl GithubFetcher for StaticFixtureFetcher {
    fn fetch(&self, _request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.document.clone())
    }
}

struct ScriptedDownloader {
    calls: Mutex<Vec<(String, usize)>>,
    outcomes: BTreeMap<String, Result<Option<DownloadedGithubImage>, String>>,
}

impl ScriptedDownloader {
    fn new(outcomes: BTreeMap<String, Result<Option<DownloadedGithubImage>, String>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            outcomes,
        }
    }

    fn calls(&self) -> Vec<(String, usize)> {
        self.calls.lock().clone()
    }
}

impl GithubImageDownloader for ScriptedDownloader {
    fn download(
        &self,
        url: &Url,
        maximum_bytes: usize,
    ) -> Result<Option<DownloadedGithubImage>, String> {
        let source = url.to_string();
        self.calls.lock().push((source.clone(), maximum_bytes));
        self.outcomes.get(&source).cloned().unwrap_or(Ok(None))
    }
}

fn successful_download(url: &str, mime: &str, bytes: Vec<u8>) -> DownloadedGithubImage {
    DownloadedGithubImage {
        final_url: Url::parse(url).expect("parse fixture image URL"),
        mime: mime.to_string(),
        bytes,
    }
}

fn png_bytes() -> Vec<u8> {
    PNG_SIGNATURE.to_vec()
}

fn oversized_png() -> Vec<u8> {
    let mut bytes = vec![0; MAX_GITHUB_IMAGE_ATTACHMENT_BYTES + 1];
    bytes[..PNG_SIGNATURE.len()].copy_from_slice(PNG_SIGNATURE);
    bytes
}

fn complete_read(start: GithubReadStart) -> GithubReadCompletion {
    match start {
        GithubReadStart::Immediate(completion) => completion,
        GithubReadStart::Deferred(deferred) => {
            // The deferred read spawns the fixture `gh` process; a fixed
            // iteration count was a ~1 s scheduler-time bound that a loaded
            // Linux runner missed with the read intact (train 44). Wait on the
            // wall clock with a liveness ceiling instead.
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                if let Some(completion) = deferred.try_complete() {
                    return completion.expect("fixture GitHub read succeeds");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("fixture GitHub read did not complete within 30s");
        }
    }
}

#[test]
fn sequential_reads_always_fetch_live_and_refresh_the_fallback_copy() {
    let storage = tempfile::tempdir().expect("create cache storage");
    let fetcher = Arc::new(ProbeFetcher::ordinary());
    let engine = GithubReadEngine::new(
        sqlite_cache_store(storage.path().join("aft.db")),
        fetcher.clone(),
        Arc::new(ScriptedDownloader::new(BTreeMap::new())),
        Arc::new(FixtureClock::new(1_000)),
    );

    let first = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                RESOURCE,
                "/fixture/cache",
                "principal:alice",
                None,
                GithubReadSelector::WholeDocument,
            )
            .expect("start first live read"),
    );
    let second = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                RESOURCE,
                "/fixture/cache",
                "principal:alice",
                None,
                GithubReadSelector::WholeDocument,
            )
            .expect("start second live read"),
    );

    assert_eq!(first.freshness, GithubReadFreshness::Fetched);
    assert_eq!(second.freshness, GithubReadFreshness::Fetched);
    assert!(second.content.contains("network revision 2"));
    assert_eq!(
        fetcher.calls(),
        2,
        "two sequential reads must issue two live fetches"
    );
}

#[test]
fn live_fetches_keep_authentication_and_unresolved_short_contexts_isolated() {
    let storage = tempfile::tempdir().expect("create cache storage");
    let fetcher = Arc::new(ProbeFetcher::ordinary());
    let engine = GithubReadEngine::new(
        sqlite_cache_store(storage.path().join("aft.db")),
        fetcher.clone(),
        Arc::new(ScriptedDownloader::new(BTreeMap::new())),
        Arc::new(FixtureClock::new(1_000)),
    );

    let alice = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                RESOURCE,
                "/fixture/authentication",
                "principal:alice",
                None,
                GithubReadSelector::WholeDocument,
            )
            .expect("start Alice read"),
    );
    let bob = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                RESOURCE,
                "/fixture/authentication",
                "principal:bob",
                None,
                GithubReadSelector::WholeDocument,
            )
            .expect("start Bob read"),
    );
    assert_eq!(fetcher.calls(), 2);
    assert_ne!(alice.content, bob.content);

    let first_short = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                "issue://7",
                "/fixture/worktree-a",
                "principal:alice",
                None,
                GithubReadSelector::WholeDocument,
            )
            .expect("resolve first short resource"),
    );
    let second_short = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                "issue://7",
                "/fixture/worktree-b",
                "principal:alice",
                None,
                GithubReadSelector::WholeDocument,
            )
            .expect("resolve second short resource"),
    );
    assert_eq!(fetcher.calls(), 4);
    assert!(first_short.content.contains("Repository: owner/worktree-a"));
    assert!(second_short
        .content
        .contains("Repository: owner/worktree-b"));

    for (resource, working_directory, identity) in [
        (RESOURCE, "/fixture/authentication", "principal:alice"),
        (RESOURCE, "/fixture/authentication", "principal:bob"),
        ("issue://7", "/fixture/worktree-a", "principal:alice"),
        ("issue://7", "/fixture/worktree-b", "principal:alice"),
    ] {
        complete_read(
            engine
                .start_resource(
                    &enabled_gh_read(),
                    resource,
                    working_directory,
                    identity,
                    None,
                    GithubReadSelector::WholeDocument,
                )
                .expect("repeat live read"),
        );
    }
    assert_eq!(
        fetcher.calls(),
        8,
        "cached fallback copies must not suppress live reads"
    );
}

#[test]
fn successful_same_resource_invalidation_removes_the_disclosed_fallback_copy() {
    let storage = tempfile::tempdir().expect("create cache storage");
    let fetcher = Arc::new(FailingAfterFirstFetcher(AtomicUsize::new(0)));
    let engine = GithubReadEngine::new(
        sqlite_cache_store(storage.path().join("aft.db")),
        fetcher.clone(),
        Arc::new(ScriptedDownloader::new(BTreeMap::new())),
        Arc::new(FixtureClock::new(1_234)),
    );

    complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                RESOURCE,
                "/fixture/mutation",
                "principal:alice",
                None,
                GithubReadSelector::WholeDocument,
            )
            .expect("seed fallback copy"),
    );

    // A failed mutation has no success-only invalidation call, so its fallback
    // must remain available and visibly disclosed when the next live fetch fails.
    let fallback = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                RESOURCE,
                "/fixture/mutation",
                "principal:alice",
                None,
                GithubReadSelector::WholeDocument,
            )
            .expect("attempt live read after failed mutation"),
    );
    assert_eq!(fallback.freshness, GithubReadFreshness::CachedFallback);
    assert!(fallback.content.starts_with(
        "[cached copy from 1970-01-01T00:00:01.234Z; live fetch failed: fixture live fetch failed]\n"
    ));

    assert_eq!(
        engine
            .invalidate(GithubResourceKind::Issue, "owner/repo", 8, None)
            .expect("invalidate unrelated mutation resource"),
        0
    );
    assert!(complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                RESOURCE,
                "/fixture/mutation",
                "principal:alice",
                None,
                GithubReadSelector::WholeDocument,
            )
            .expect("unrelated mutation leaves fallback copy"),
    )
    .content
    .starts_with("[cached copy from"));

    assert_eq!(
        engine
            .invalidate(GithubResourceKind::Issue, "owner/repo", 7, None)
            .expect("apply successful same-resource mutation invalidation"),
        1
    );
    let error = match engine
        .start_resource(
            &enabled_gh_read(),
            RESOURCE,
            "/fixture/mutation",
            "principal:alice",
            None,
            GithubReadSelector::WholeDocument,
        )
        .expect("attempt read after matching invalidation")
    {
        GithubReadStart::Deferred(deferred) => {
            let result = (0..1_000)
                .find_map(|_| {
                    let result = deferred.try_complete();
                    if result.is_none() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    result
                })
                .expect("GitHub read did not complete");
            result.expect_err("invalidated fallback must not hide fetch failure")
        }
        GithubReadStart::Immediate(_) => panic!("every read must attempt a live fetch"),
    };
    assert_eq!(
        error,
        GithubReadError::FetchFailed("fixture live fetch failed".to_string())
    );
    assert_eq!(fetcher.0.load(Ordering::SeqCst), 4);
}

#[test]
fn vision_capability_preserves_bug_report_text_and_only_downloads_for_explicit_true() {
    let storage = tempfile::tempdir().expect("create cache storage");
    let first = "https://user-images.githubusercontent.com/1234/first.png";
    let second = "https://github.com/user-attachments/files/73/second.png";
    let outcomes = BTreeMap::from([
        (
            first.to_string(),
            Ok(Some(successful_download(first, "image/png", png_bytes()))),
        ),
        (
            second.to_string(),
            Ok(Some(successful_download(second, "image/png", png_bytes()))),
        ),
    ]);
    let fetcher = Arc::new(StaticFixtureFetcher::new(bug_report_fixture()));
    let downloader = Arc::new(ScriptedDownloader::new(outcomes));
    let engine = GithubReadEngine::new(
        sqlite_cache_store(storage.path().join("aft.db")),
        fetcher.clone(),
        downloader.clone(),
        Arc::new(FixtureClock::new(1_000)),
    );

    let missing_capability = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                "issue://cortexkit/aft/73",
                "/fixture/vision",
                "principal:vision",
                None,
                GithubReadSelector::WholeDocument,
            )
            .expect("read fixture without capability"),
    );
    assert!(missing_capability.content.contains(first));
    assert!(missing_capability.content.contains(second));
    assert!(missing_capability.attachments.is_empty());
    assert!(downloader.calls().is_empty());

    let false_capability = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                "issue://cortexkit/aft/73",
                "/fixture/vision",
                "principal:vision",
                Some(false),
                GithubReadSelector::WholeDocument,
            )
            .expect("read fixture with false capability"),
    );
    assert_eq!(false_capability.content, missing_capability.content);
    assert!(false_capability.attachments.is_empty());
    assert!(downloader.calls().is_empty());

    let explicit_vision = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                "issue://cortexkit/aft/73",
                "/fixture/vision",
                "principal:vision",
                Some(true),
                GithubReadSelector::WholeDocument,
            )
            .expect("read fixture with explicit vision capability"),
    );
    assert_eq!(explicit_vision.content, missing_capability.content);
    assert_eq!(
        fetcher.calls(),
        3,
        "each capability mode must fetch live before rendering attachments"
    );
    assert_eq!(
        explicit_vision
            .attachments
            .iter()
            .map(|attachment| attachment.source_url.as_str())
            .collect::<Vec<_>>(),
        [first, second],
        "eligible attachments must follow document order across both allowed host forms"
    );
    assert_eq!(
        downloader.calls(),
        vec![
            (first.to_string(), MAX_GITHUB_IMAGE_ATTACHMENT_BYTES),
            (
                second.to_string(),
                MAX_GITHUB_IMAGE_ATTACHMENT_BYTES - PNG_SIGNATURE.len(),
            ),
        ]
    );
}

#[test]
fn invalid_or_failed_attachment_candidates_leave_the_text_read_complete_without_partial_data() {
    let storage = tempfile::tempdir().expect("create cache storage");
    let unsupported = "https://github.com/user-attachments/files/73/unsupported.svg";
    let redirected = "https://user-images.githubusercontent.com/1234/redirected.png";
    let failed = "https://github.com/user-attachments/files/73/failure.png";
    let oversized = "https://user-images.githubusercontent.com/1234/oversized.png";
    let malformed = "https://github.com:99999/user-attachments/malformed.png";
    let ineligible_host = "https://example.test/bug.png";
    let ineligible_github_path = "https://github.com/cortexkit/aft/blob/main/bug.png";
    let document = GithubDocument {
        repository: "owner/repo".to_string(),
        kind: GithubDocumentKind::Issue,
        number: 7,
        title: "attachment failure fixture".to_string(),
        state: "OPEN".to_string(),
        body: [
            unsupported,
            redirected,
            failed,
            oversized,
            malformed,
            ineligible_host,
            ineligible_github_path,
        ]
        .join("\n"),
        ..GithubDocument::default()
    };
    let outcomes = BTreeMap::from([
        (
            unsupported.to_string(),
            Ok(Some(successful_download(
                unsupported,
                "image/svg+xml",
                png_bytes(),
            ))),
        ),
        (
            redirected.to_string(),
            Ok(Some(successful_download(
                "https://example.test/redirected.png",
                "image/png",
                png_bytes(),
            ))),
        ),
        (
            failed.to_string(),
            Err("fixture image host failed".to_string()),
        ),
        (
            oversized.to_string(),
            Ok(Some(successful_download(
                oversized,
                "image/png",
                oversized_png(),
            ))),
        ),
    ]);
    let downloader = Arc::new(ScriptedDownloader::new(outcomes));
    let engine = GithubReadEngine::new(
        sqlite_cache_store(storage.path().join("aft.db")),
        Arc::new(StaticFixtureFetcher::new(document)),
        downloader.clone(),
        Arc::new(FixtureClock::new(1_000)),
    );

    let completion = complete_read(
        engine
            .start_resource(
                &enabled_gh_read(),
                RESOURCE,
                "/fixture/attachment-failures",
                "principal:vision",
                Some(true),
                GithubReadSelector::WholeDocument,
            )
            .expect("start attachment failure fixture read"),
    );
    assert!(completion.attachments.is_empty());
    for url in [
        unsupported,
        redirected,
        failed,
        oversized,
        malformed,
        ineligible_host,
        ineligible_github_path,
    ] {
        assert!(completion.content.contains(url), "text keeps {url}");
    }
    assert_eq!(
        downloader.calls(),
        vec![
            (unsupported.to_string(), MAX_GITHUB_IMAGE_ATTACHMENT_BYTES),
            (redirected.to_string(), MAX_GITHUB_IMAGE_ATTACHMENT_BYTES),
            (failed.to_string(), MAX_GITHUB_IMAGE_ATTACHMENT_BYTES),
            (oversized.to_string(), MAX_GITHUB_IMAGE_ATTACHMENT_BYTES),
        ],
        "malformed and ineligible URLs must never reach the image network seam"
    );
}

#[test]
fn attachment_count_and_aggregate_byte_budgets_stop_before_exposing_partial_images() {
    let count_urls = (0..=MAX_GITHUB_IMAGE_ATTACHMENTS)
        .map(|index| format!("https://user-images.githubusercontent.com/1234/count-{index}.png"))
        .collect::<Vec<_>>();
    let count_outcomes = count_urls
        .iter()
        .map(|url| {
            (
                url.clone(),
                Ok(Some(successful_download(url, "image/png", png_bytes()))),
            )
        })
        .collect();
    let count_downloader = ScriptedDownloader::new(count_outcomes);
    let count_attachments =
        download_github_image_attachments(&count_urls.join("\n"), &count_downloader);
    assert_eq!(count_attachments.len(), MAX_GITHUB_IMAGE_ATTACHMENTS);
    assert_eq!(
        count_attachments
            .iter()
            .map(|attachment| attachment.source_url.as_str())
            .collect::<Vec<_>>(),
        count_urls[..MAX_GITHUB_IMAGE_ATTACHMENTS]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    assert_eq!(count_downloader.calls().len(), MAX_GITHUB_IMAGE_ATTACHMENTS);

    let first = "https://github.com/user-attachments/files/73/almost-full.png";
    let second = "https://user-images.githubusercontent.com/1234/remainder.png";
    let third = "https://github.com/user-attachments/files/73/never-requested.png";
    let mut almost_full_png = vec![0; MAX_GITHUB_IMAGE_ATTACHMENT_BYTES - PNG_SIGNATURE.len()];
    almost_full_png[..PNG_SIGNATURE.len()].copy_from_slice(PNG_SIGNATURE);
    let aggregate_outcomes = BTreeMap::from([
        (
            first.to_string(),
            Ok(Some(successful_download(
                first,
                "image/png",
                almost_full_png,
            ))),
        ),
        (
            second.to_string(),
            Ok(Some(successful_download(second, "image/png", png_bytes()))),
        ),
        (
            third.to_string(),
            Ok(Some(successful_download(third, "image/png", png_bytes()))),
        ),
    ]);
    let aggregate_downloader = ScriptedDownloader::new(aggregate_outcomes);
    let aggregate_attachments = download_github_image_attachments(
        &[first, second, third].join("\n"),
        &aggregate_downloader,
    );
    assert_eq!(aggregate_attachments.len(), 2);
    assert_eq!(
        aggregate_attachments
            .iter()
            .map(|attachment| attachment.bytes.len())
            .sum::<usize>(),
        MAX_GITHUB_IMAGE_ATTACHMENT_BYTES
    );
    assert_eq!(
        aggregate_downloader.calls(),
        vec![
            (first.to_string(), MAX_GITHUB_IMAGE_ATTACHMENT_BYTES),
            (second.to_string(), PNG_SIGNATURE.len()),
        ],
        "an exhausted aggregate budget must skip later downloads instead of exposing partial data"
    );

    let rendered = render_document(&GithubDocument {
        repository: "owner/repo".to_string(),
        kind: GithubDocumentKind::Issue,
        number: 7,
        title: "bounded attachment fixture".to_string(),
        state: "OPEN".to_string(),
        body: [first, second, third].join("\n"),
        ..GithubDocument::default()
    });
    assert!(
        rendered.contains(third),
        "attachment bounds never remove text URLs"
    );
}
