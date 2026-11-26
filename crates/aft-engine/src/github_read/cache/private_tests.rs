use super::*;
use crate::github_read::attachments::DownloadedGithubImage;
use crate::github_read::model::GithubDocumentKind;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

#[test]
fn a_line_page_stops_at_the_file_read_byte_and_line_bounds() {
    // 100 lines of 1 KB: twice the page ceiling, well under the line limit.
    let line = "x".repeat(1023);
    let canonical = format!("{line}\n").repeat(100);
    let page = GithubReadSelector::LineRange {
        start_line: 1,
        end_line: None,
        limit: 2000,
    };
    let (content, cut) = select(&canonical, page.clone());
    assert!(cut);
    assert!(content.len() <= MAX_PAGE_BYTES + 100, "{}", content.len());
    assert!(content
        .ends_with("... (output truncated at 50KB, use start_line/end_line to read sections)\n"));
    let completion = complete(
        canonical,
        page.clone(),
        Vec::new(),
        DocumentSource::Fetched(&GithubDocument::default()),
    );
    assert!(completion.page_cut_by_size);
    assert_eq!(completion.total_lines, 100);

    // One 200 KB line (a pasted log) is cut like a long file line.
    let (content, cut) = select(&format!("{}\n", "y".repeat(200 * 1024)), page);
    assert!(!cut);
    assert_eq!(content, format!("{}... (truncated)\n", "y".repeat(2000)));
}

#[test]
fn a_line_page_under_the_bounds_is_unchanged() {
    let canonical = "one\ntwo\nthree\n";
    let page = |start_line, end_line| GithubReadSelector::LineRange {
        start_line,
        end_line,
        limit: 2000,
    };
    assert_eq!(
        select(canonical, page(1, None)),
        (canonical.to_string(), false)
    );
    assert_eq!(select(canonical, page(2, Some(2))).0, "two\n");
    assert_eq!(select(canonical, page(4, None)).0, "");
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
        document_gap: bool,
    ) -> Result<(), String> {
        *self.0.lock() = Some(GithubReadCacheEntry {
            canonical_text: canonical_text.to_string(),
            document_gap: Some(document_gap),
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

struct FixtureClock(AtomicI64);

impl FixtureClock {
    fn new(now: i64) -> Self {
        Self(AtomicI64::new(now))
    }
}

impl GithubReadClock for FixtureClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct FixtureFetcher(AtomicUsize);

impl GithubFetcher for FixtureFetcher {
    fn fetch(&self, request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(GithubDocument {
            repository: request
                .resource
                .repository
                .clone()
                .unwrap_or_else(|| "owner/repo".to_string()),
            kind: GithubDocumentKind::Issue,
            number: request.resource.number,
            title: "fixture".to_string(),
            state: "OPEN".to_string(),
            body: "body https://user-images.githubusercontent.com/fixture.png".to_string(),
            ..GithubDocument::default()
        })
    }
}

#[derive(Default)]
struct CountingDownloader(AtomicUsize);

impl GithubImageDownloader for CountingDownloader {
    fn download(
        &self,
        _url: &url::Url,
        _maximum_bytes: usize,
    ) -> Result<Option<DownloadedGithubImage>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

fn enabled_gh_read() -> GithubConfig {
    GithubConfig {
        read: true,
        ..GithubConfig::default()
    }
}

fn wait_for(deferred: GithubReadDeferred) -> Result<GithubReadCompletion, GithubReadError> {
    for _ in 0..1000 {
        if let Some(result) = deferred.try_complete() {
            return result;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("deferred read did not complete")
}

fn fetched_text(start: GithubReadStart) -> String {
    match start {
        GithubReadStart::Deferred(deferred) => wait_for(deferred).unwrap().content,
        GithubReadStart::Immediate(_) => panic!("live reads must be deferred"),
    }
}

/// Holds its first fetch until released. A shorthand read resolves to
/// `team/new`, the checkout's current default repository.
struct RepositoryFetcher {
    calls: AtomicUsize,
    started: std::sync::mpsc::SyncSender<()>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl GithubFetcher for RepositoryFetcher {
    fn fetch(&self, request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let _ = self.started.send(());
        if let Some(release) = self.release.lock().take() {
            release.recv().expect("release live fetch");
        }
        Ok(GithubDocument {
            repository: request
                .resource
                .repository
                .clone()
                .unwrap_or_else(|| "team/new".to_string()),
            kind: GithubDocumentKind::Issue,
            number: request.resource.number,
            title: "fixture".to_string(),
            state: "OPEN".to_string(),
            ..GithubDocument::default()
        })
    }
}

#[test]
fn a_stale_shorthand_alias_does_not_join_another_repositorys_live_fetch() {
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(8);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let fetcher = Arc::new(RepositoryFetcher {
        calls: AtomicUsize::new(0),
        started: started_tx,
        release: Mutex::new(Some(release_rx)),
    });
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        fetcher.clone(),
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(2_000)),
    );
    // An earlier shorthand read here resolved to team/old; the checkout's
    // default repository has since changed to team/new.
    let shorthand = GithubReadRequest::parse("issue://42", "/fixture", "identity", None).unwrap();
    engine
        .state
        .lock()
        .aliases
        .insert(short_alias(&shorthand), "team/old".to_string());

    let explicit = engine
        .start(
            &enabled_gh_read(),
            GithubReadRequest::parse("issue://team/old/42", "/fixture", "identity", None).unwrap(),
            GithubReadSelector::default(),
        )
        .unwrap();
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("explicit live fetch started");
    let shorthand = engine
        .start(&enabled_gh_read(), shorthand, GithubReadSelector::default())
        .unwrap();
    release_tx.send(()).expect("release the explicit fetch");

    assert!(fetched_text(explicit).contains("Repository: team/old\n"));
    assert!(fetched_text(shorthand).contains("Repository: team/new\n"));
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn shorthand_aliases_do_not_grow_with_sessions() {
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        Arc::new(FixtureFetcher::default()),
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(2_000)),
    );
    for session in 0..100 {
        let request =
            GithubReadRequest::parse("issue://1", "/fixture", format!("session:{session}"), None)
                .unwrap();
        let start = engine
            .start(&enabled_gh_read(), request, GithubReadSelector::default())
            .unwrap();
        fetched_text(start);
    }
    assert_eq!(engine.state.lock().aliases.len(), 1);
}
