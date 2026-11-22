use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use crate::config::GithubConfig;
use crate::db::github_read_cache::{GithubReadCacheEntry, GithubReadCacheKey};
use crate::github_read::{DownloadedGithubImage, GithubImageDownloader};
use crate::github_read::{
    GithubDiff, GithubFetchRequest, GithubFetcher, GithubReadCacheStore, GithubReadClock,
    GithubReadCompletion, GithubReadDeferred, GithubReadEngine, GithubReadError,
    GithubReadFreshness, GithubReadRequest, GithubReadSelector, GithubReadStart, GithubReadView,
    GithubResourceKind, SqliteGithubReadCacheStore,
};
use crate::github_read::{GithubDocument, GithubDocumentKind};
use parking_lot::Mutex;
use std::sync::{mpsc, Arc};

#[test]
fn sqlite_store_reuses_the_resident_connection_and_creates_its_table_once() {
    let storage = tempfile::tempdir().unwrap();
    let database = storage.path().join("aft.db");
    let resident =
        Arc::new(std::sync::Mutex::new(crate::db::open(&database).unwrap()));
    let store = SqliteGithubReadCacheStore::new(&database);
    store.attach_resident_connection(Some(&resident));
    let key = GithubReadCacheKey::new(GithubReadResourceKind::Issue, "owner/repo", 7, "id");

    // Every operation used to open the database again: a read-only schema
    // peek, PRAGMAs and the migration check, on the request thread.
    let peeks = crate::db::schema_peek_count_for_test();
    assert_eq!(store.lookup(&key).unwrap(), None);
    store.upsert(&key, "# Issue\n", 1_000, false).unwrap();
    assert_eq!(
        store.lookup(&key).unwrap().unwrap().canonical_text,
        "# Issue\n"
    );
    assert_eq!(
        store
            .invalidate(GithubResourceKind::Issue, "owner/repo", 7, Some("id"))
            .unwrap(),
        1
    );
    assert_eq!(crate::db::schema_peek_count_for_test(), peeks);

    // The table is created once per connection, not before every
    // statement: dropped behind the store's back, the next statement
    // fails, and the one after creates it again.
    resident
        .lock()
        .unwrap()
        .execute_batch("DROP TABLE github_read_cache")
        .unwrap();
    assert!(store.lookup(&key).is_err());
    assert_eq!(store.lookup(&key).unwrap(), None);

    // Once the context lets its connection go, the store opens its own.
    drop(resident);
    assert_eq!(store.lookup(&key).unwrap(), None);
    assert!(crate::db::schema_peek_count_for_test() > peeks);
}

#[test]
fn sqlite_store_reuses_legacy_lookup_schema_after_a_failed_migration() {
    let storage = tempfile::tempdir().unwrap();
    let database = storage.path().join("aft.db");
    let connection = crate::db::open(&database).unwrap();
    let key = GithubReadCacheKey::new(GithubReadResourceKind::Issue, "owner/repo", 7, "id");
    crate::db::github_read_cache::upsert_github_read_cache_entry(
        &connection,
        &key,
        "# Legacy",
        1_000,
        false,
    )
    .unwrap();
    connection
        .execute_batch(
            "ALTER TABLE github_read_cache DROP COLUMN document_gap_with_text_hash;
             PRAGMA query_only = ON;",
        )
        .unwrap();
    let resident = Arc::new(std::sync::Mutex::new(connection));
    let store = SqliteGithubReadCacheStore::new(&database);
    store.attach_resident_connection(Some(&resident));

    let entry = store.lookup(&key).unwrap().unwrap();
    assert_eq!(entry.canonical_text, "# Legacy");
    assert_eq!(entry.document_gap, None);
    assert!(store.upsert(&key, "# Updated", 2_000, true).is_err());

    resident
        .lock()
        .unwrap()
        .execute_batch("PRAGMA query_only = OFF")
        .unwrap();
    assert_eq!(store.lookup(&key).unwrap(), Some(entry));
    // A warm lookup uses the saved legacy schema instead of retrying migration.
    assert!(resident
        .lock()
        .unwrap()
        .prepare("SELECT document_gap_with_text_hash FROM github_read_cache")
        .is_err());

    store.upsert(&key, "# Updated", 2_000, true).unwrap();
    let entry = store.lookup(&key).unwrap().unwrap();
    assert_eq!(entry.canonical_text, "# Updated");
    assert_eq!(entry.document_gap, Some(true));
}

#[derive(Default)]
struct MemoryCache(Mutex<Option<GithubReadCacheEntry>>);

impl MemoryCache {
    fn with_entry(canonical_text: &str, fetched_at_ms: i64) -> Self {
        Self(Mutex::new(Some(GithubReadCacheEntry {
            canonical_text: canonical_text.to_string(),
            document_gap: Some(false),
            fetched_at_ms,
            updated_at_ms: fetched_at_ms,
        })))
    }
}

impl GithubReadCacheStore for MemoryCache {
    fn lookup(
        &self,
        _key: &GithubReadCacheKey,
    ) -> Result<Option<GithubReadCacheEntry>, String> {
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

struct FailingFetcher;

impl GithubFetcher for FailingFetcher {
    fn fetch(&self, _request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
        Err(GithubReadError::FetchFailed(
            "fixture GitHub fetch failed".to_string(),
        ))
    }
}

struct GatedFetcher {
    calls: AtomicUsize,
    started: std::sync::mpsc::SyncSender<()>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl GithubFetcher for GatedFetcher {
    fn fetch(&self, request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.send(()).expect("report live fetch start");
        if let Some(release) = self.release.lock().take() {
            release.recv().expect("release live fetch");
        }
        Ok(GithubDocument {
            repository: "owner/repo".to_string(),
            kind: GithubDocumentKind::Issue,
            number: request.resource.number,
            title: "fixture".to_string(),
            state: "OPEN".to_string(),
            body: "body".to_string(),
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

fn request(vision_capability: Option<bool>) -> GithubReadRequest {
    GithubReadRequest::parse("issue://1", "/fixture", "identity", vision_capability).unwrap()
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

#[test]
fn diff_reads_fetch_live_without_cache_or_attachments() {
    struct DiffFetcher(AtomicUsize);
    impl GithubFetcher for DiffFetcher {
        fn fetch(&self, _: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
            panic!("diff must not fetch a document");
        }
        fn fetch_diff(
            &self,
            _: &GithubFetchRequest,
        ) -> Result<GithubDiff, GithubReadError> {
            let revision = self.0.fetch_add(1, Ordering::SeqCst);
            if revision >= 2 {
                return Err(GithubReadError::FetchFailed(
                    "live diff unavailable".to_string(),
                ));
            }
            Ok(GithubDiff {
                header: format!("PR owner/repo#42 head {revision}"),
                body: "+change\n".to_string(),
                cap_hit: false,
            })
        }
    }
    let cache = Arc::new(MemoryCache::with_entry("ordinary document", 1));
    let downloader = Arc::new(CountingDownloader::default());
    let engine = GithubReadEngine::new(
        cache.clone(),
        Arc::new(DiffFetcher(AtomicUsize::new(0))),
        downloader.clone(),
        Arc::new(FixtureClock::new(1)),
    );
    for revision in 0..3 {
        let started = engine
            .start_resource(
                &enabled_gh_read(),
                "pr://owner/repo/42/diff",
                "fixture",
                "identity",
                Some(true),
                GithubReadSelector::WholeDocument,
            )
            .unwrap();
        let GithubReadStart::Deferred(deferred) = started else {
            panic!("live diff must defer");
        };
        let result = wait_for(deferred);
        if revision < 2 {
            assert!(result
                .unwrap()
                .content
                .contains(&format!("head {revision}")));
        } else {
            assert_eq!(result.unwrap_err().to_string(), "live diff unavailable");
        }
    }
    assert_eq!(
        cache.0.lock().as_ref().unwrap().canonical_text,
        "ordinary document"
    );
    assert_eq!(downloader.0.load(Ordering::SeqCst), 0);
}

#[test]
fn diff_outline_refuses_before_fetching() {
    let fetcher = Arc::new(FixtureFetcher::default());
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        fetcher.clone(),
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(1)),
    );
    let result = engine.start_resource_with_view(
        &enabled_gh_read(),
        "pr://42/diff",
        "fixture",
        "identity",
        None,
        GithubReadSelector::WholeDocument,
        GithubReadView::Outline,
    );
    assert!(result
        .err()
        .unwrap()
        .to_string()
        .contains("Use read for diffs"));
    assert_eq!(fetcher.0.load(Ordering::SeqCst), 0);
}

#[test]
fn disabled_read_refuses_before_the_fetch_seam_runs() {
    let fetcher = Arc::new(FixtureFetcher::default());
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        fetcher.clone(),
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(1_000)),
    );

    let error = match engine.start_resource(
        &GithubConfig::default(),
        "issue://1",
        "/fixture",
        "identity",
        None,
        GithubReadSelector::default(),
    ) {
        Err(error) => error,
        Ok(_) => panic!("disabled GitHub reads must refuse"),
    };

    assert_eq!(error.code(), "gh_read_disabled");
    assert_eq!(
        error.to_string(),
        "GitHub reads are disabled; set github.read: true in aft.jsonc"
    );
    assert_eq!(fetcher.0.load(Ordering::SeqCst), 0);
}

#[test]
fn enabled_read_reaches_the_fixture_fetcher() {
    let fetcher = Arc::new(FixtureFetcher::default());
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        fetcher.clone(),
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(1_000)),
    );

    let GithubReadStart::Deferred(deferred) = engine
        .start_resource(
            &enabled_gh_read(),
            "issue://1",
            "/fixture",
            "identity",
            None,
            GithubReadSelector::default(),
        )
        .expect("enabled GitHub reads should proceed")
    else {
        panic!("a cache miss must defer its GitHub fetch");
    };

    assert!(wait_for(deferred).unwrap().content.contains("# Issue #1"));
    assert_eq!(fetcher.0.load(Ordering::SeqCst), 1);
}

#[test]
fn every_read_fetches_live_before_optional_attachments() {
    let cache = Arc::new(MemoryCache::default());
    let fetcher = Arc::new(FixtureFetcher::default());
    let downloader = Arc::new(CountingDownloader::default());
    let engine = GithubReadEngine::new(
        cache,
        fetcher.clone(),
        downloader.clone(),
        Arc::new(FixtureClock::new(1_000)),
    );

    let first = match engine
        .start(
            &enabled_gh_read(),
            request(None),
            GithubReadSelector::default(),
        )
        .unwrap()
    {
        GithubReadStart::Deferred(deferred) => wait_for(deferred).unwrap(),
        GithubReadStart::Immediate(_) => panic!("every GitHub read must fetch live"),
    };
    assert_eq!(first.freshness, GithubReadFreshness::Fetched);
    assert_eq!(first.attachments.len(), 0);

    let second = match engine
        .start(
            &enabled_gh_read(),
            request(None),
            GithubReadSelector::default(),
        )
        .unwrap()
    {
        GithubReadStart::Deferred(deferred) => wait_for(deferred).unwrap(),
        GithubReadStart::Immediate(_) => panic!("cached data must not satisfy a read"),
    };
    assert_eq!(second.freshness, GithubReadFreshness::Fetched);
    assert_eq!(fetcher.0.load(Ordering::SeqCst), 2);
    assert_eq!(downloader.0.load(Ordering::SeqCst), 0);

    let vision = match engine
        .start(
            &enabled_gh_read(),
            request(Some(true)),
            GithubReadSelector::default(),
        )
        .unwrap()
    {
        GithubReadStart::Deferred(deferred) => wait_for(deferred).unwrap(),
        GithubReadStart::Immediate(_) => panic!("vision reads must fetch live"),
    };
    assert!(vision.attachments.is_empty());
    assert_eq!(fetcher.0.load(Ordering::SeqCst), 3);
    assert_eq!(downloader.0.load(Ordering::SeqCst), 1);
}

#[test]
fn failed_live_fetch_returns_a_loudly_disclosed_cached_fallback() {
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::with_entry("# Cached issue\n", 1_234)),
        Arc::new(FailingFetcher),
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(2_000)),
    );

    let completion = match engine
        .start(
            &enabled_gh_read(),
            GithubReadRequest::parse("issue://owner/repo/1", "/fixture", "identity", None)
                .unwrap(),
            GithubReadSelector::default(),
        )
        .unwrap()
    {
        GithubReadStart::Deferred(deferred) => wait_for(deferred).unwrap(),
        GithubReadStart::Immediate(_) => panic!("fallback requires a live fetch attempt"),
    };

    assert_eq!(completion.freshness, GithubReadFreshness::CachedFallback);
    assert!(completion.content.starts_with(
        "[cached copy from 1970-01-01T00:00:01.234Z; live fetch failed: fixture GitHub fetch failed]\n"
    ));
    assert!(completion.content.ends_with("# Cached issue\n"));
}

#[test]
fn github_read_pr_fallback_discloses_stale_threads_without_changing_cache() {
    let cached = "# Cached pull request\nReview threads: 0 open, 1 resolved\n";
    let cache = Arc::new(MemoryCache::with_entry(cached, 1_234));
    let engine = GithubReadEngine::new(
        cache.clone(),
        Arc::new(FailingFetcher),
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(2_000)),
    );
    let deferred = match engine
        .start(
            &enabled_gh_read(),
            GithubReadRequest::parse("pr://owner/repo/1", "/fixture", "identity", None)
                .unwrap(),
            GithubReadSelector::default(),
        )
        .unwrap()
    {
        GithubReadStart::Deferred(deferred) => deferred,
        GithubReadStart::Immediate(_) => panic!("fallback requires a live fetch attempt"),
    };
    let late = deferred
        .late_answer_for_test()
        .answer(std::time::Duration::from_secs(1))
        .unwrap();
    let completion = wait_for(deferred).unwrap();
    for answer in [late, completion] {
        assert_eq!(answer.freshness, GithubReadFreshness::CachedFallback);
        assert_eq!(
            answer
                .content
                .matches(
                    aft_engine::test_bridge::search_misc::github_read::stale_thread_note_for_test()
                )
                .count(),
            1
        );
        assert!(answer.content.ends_with(cached));
    }
    assert_eq!(cache.0.lock().as_ref().unwrap().canonical_text, cached);
}

#[test]
fn failed_live_fetch_without_a_cached_copy_preserves_its_typed_error() {
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        Arc::new(FailingFetcher),
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(2_000)),
    );

    let error = match engine
        .start(
            &enabled_gh_read(),
            request(None),
            GithubReadSelector::default(),
        )
        .unwrap()
    {
        GithubReadStart::Deferred(deferred) => wait_for(deferred).unwrap_err(),
        GithubReadStart::Immediate(_) => panic!("every read must attempt a live fetch"),
    };

    assert_eq!(
        error,
        GithubReadError::FetchFailed("fixture GitHub fetch failed".to_string())
    );
}

#[tokio::test]
async fn deferred_github_read_failure_notifies_completion() {
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        Arc::new(FailingFetcher),
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(1_000)),
    );
    let wake = crate::pending_response::DeferredResponseWake::default();
    let pending = {
        let _scope = wake.install();
        let GithubReadStart::Deferred(pending) = engine
            .start(
                &enabled_gh_read(),
                request(None),
                GithubReadSelector::WholeDocument,
            )
            .expect("start failing read")
        else {
            panic!("live fetch must defer")
        };
        pending
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), wake.notified())
        .await
        .expect("GitHub read failure wake");
    assert_eq!(wake.generation(), 1);
    assert!(matches!(
        pending.try_complete(),
        Some(Err(GithubReadError::FetchFailed(_)))
    ));
}

#[tokio::test]
async fn deferred_github_read_notifies_each_waiter_after_completion() {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let fetcher = Arc::new(GatedFetcher {
        calls: AtomicUsize::new(0),
        started: started_tx,
        release: Mutex::new(Some(release_rx)),
    });
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        fetcher.clone(),
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(1_000)),
    );
    let first_wake = crate::pending_response::DeferredResponseWake::default();
    let second_wake = crate::pending_response::DeferredResponseWake::default();
    let start = |wake: &crate::pending_response::DeferredResponseWake| {
        let _scope = wake.install();
        let GithubReadStart::Deferred(pending) = engine
            .start(
                &enabled_gh_read(),
                request(None),
                GithubReadSelector::WholeDocument,
            )
            .expect("start deferred read")
        else {
            panic!("live fetch must defer")
        };
        pending
    };
    let first = start(&first_wake);
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("fetch starts");
    let second = start(&second_wake);
    release_tx.send(()).expect("release shared fetch");
    // No polling fallback: both connections must be notified even though
    // only the first caller started the shared producer thread.
    for (wake, pending) in [(&first_wake, first), (&second_wake, second)] {
        tokio::time::timeout(std::time::Duration::from_secs(5), wake.notified())
            .await
            .expect("GitHub read completion wake");
        assert_eq!(wake.generation(), 1);
        let completion = pending
            .try_complete()
            .expect("result queued before wake")
            .unwrap();
        assert!(completion.content.contains("fixture"));
    }
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn panicking_fetch_completes_all_waiters_and_allows_a_new_flight() {
    struct PanicOnceFetcher {
        calls: AtomicUsize,
        started: std::sync::mpsc::SyncSender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl GithubFetcher for PanicOnceFetcher {
        fn fetch(
            &self,
            request: &GithubFetchRequest,
        ) -> Result<GithubDocument, GithubReadError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.started.send(()).unwrap();
                self.release.lock().recv().unwrap();
                panic!("fixture fetch panic");
            }
            FixtureFetcher::default().fetch(request)
        }
    }
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let fetcher = Arc::new(PanicOnceFetcher {
        calls: AtomicUsize::new(0),
        started: started_tx,
        release: Mutex::new(release_rx),
    });
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        fetcher.clone(),
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(2_000)),
    );
    let start = || match engine
        .start(
            &enabled_gh_read(),
            request(None),
            GithubReadSelector::default(),
        )
        .unwrap()
    {
        GithubReadStart::Deferred(pending) => pending,
        GithubReadStart::Immediate(_) => panic!("live fetch must defer"),
    };
    let first = start();
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let follower = start();
    release_tx.send(()).unwrap();
    for pending in [first, follower] {
        assert_eq!(
            wait_for(pending),
            Err(GithubReadError::FetchFailed(
                "GitHub read worker stopped before completing".to_string()
            ))
        );
    }
    assert!(engine.flights_empty_for_test());
    assert_eq!(
        wait_for(start()).unwrap().freshness,
        GithubReadFreshness::Fetched
    );
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn concurrent_same_resource_reads_share_one_live_fetch() {
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let fetcher = Arc::new(GatedFetcher {
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

    let first = engine
        .start(
            &enabled_gh_read(),
            request(None),
            GithubReadSelector::default(),
        )
        .unwrap();
    started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("first live fetch started");
    let second = engine
        .start(
            &enabled_gh_read(),
            request(None),
            GithubReadSelector::default(),
        )
        .unwrap();
    release_tx.send(()).expect("release shared fetch");

    for start in [first, second] {
        match start {
            GithubReadStart::Deferred(deferred) => {
                assert_eq!(
                    wait_for(deferred).unwrap().freshness,
                    GithubReadFreshness::Fetched
                )
            }
            GithubReadStart::Immediate(_) => panic!("live reads must be deferred"),
        }
    }
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
}


fn fetched_text(start: GithubReadStart) -> String {
    match start {
        GithubReadStart::Deferred(deferred) => wait_for(deferred).unwrap().content,
        GithubReadStart::Immediate(_) => panic!("live reads must be deferred"),
    }
}

/// Holds its first fetch until released; the body links two images.
struct ImageFetcher {
    started: std::sync::mpsc::SyncSender<()>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl GithubFetcher for ImageFetcher {
    fn fetch(&self, request: &GithubFetchRequest) -> Result<GithubDocument, GithubReadError> {
        let _ = self.started.send(());
        if let Some(release) = self.release.lock().take() {
            release.recv().expect("release live fetch");
        }
        Ok(GithubDocument {
            repository: "owner/repo".to_string(),
            kind: GithubDocumentKind::Issue,
            number: request.resource.number,
            title: "fixture".to_string(),
            state: "OPEN".to_string(),
            body: "https://user-images.githubusercontent.com/1/a.png https://user-images.githubusercontent.com/2/b.png".to_string(),
            ..GithubDocument::default()
        })
    }
}

#[test]
fn coalesced_vision_readers_download_each_image_once() {
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(8);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let downloader = Arc::new(CountingDownloader::default());
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        Arc::new(ImageFetcher {
            started: started_tx,
            release: Mutex::new(Some(release_rx)),
        }),
        downloader.clone(),
        Arc::new(FixtureClock::new(2_000)),
    );
    let start = || {
        engine
            .start(
                &enabled_gh_read(),
                request(Some(true)),
                GithubReadSelector::default(),
            )
            .unwrap()
    };
    let first = start();
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("live fetch started");
    let followers = [start(), start()];
    release_tx.send(()).expect("release the shared fetch");
    fetched_text(first);
    for follower in followers {
        fetched_text(follower);
    }
    // Two images, three readers of one fetch: two requests, not six.
    assert_eq!(downloader.0.load(Ordering::SeqCst), 2);
}


/// A gated fetcher whose live fetch stays running until the test lets it
/// finish, the sender that does so, and the receiver of its start reports
/// (kept alive so the fetcher can report).
fn running_fetch() -> (
    Arc<GatedFetcher>,
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
) {
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(8);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let fetcher = Arc::new(GatedFetcher {
        calls: AtomicUsize::new(0),
        started: started_tx,
        release: Mutex::new(Some(release_rx)),
    });
    (fetcher, release_tx, started_rx)
}

#[test]
fn a_read_still_running_at_the_deadline_answers_with_the_labelled_cached_copy() {
    let (fetcher, release, _started) = running_fetch();
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::with_entry("# Cached issue\n", 1_234)),
        fetcher,
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(2_000)),
    );
    let GithubReadStart::Deferred(pending) = engine
        .start(
            &enabled_gh_read(),
            GithubReadRequest::parse("issue://owner/repo/1", "/fixture", "identity", None)
                .unwrap(),
            GithubReadSelector::default(),
        )
        .unwrap()
    else {
        panic!("a live read defers");
    };
    let started = std::time::Instant::now();
    let completion = pending
        .wait_until(started + std::time::Duration::from_millis(200))
        .expect("the cached copy answers");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(completion.freshness, GithubReadFreshness::CachedFallback);
    assert!(
        completion.content.starts_with(
            "[cached copy from 1970-01-01T00:00:01.234Z; live fetch failed: GitHub did not answer within 1s; the read continues in the background]\n"
        ),
        "{}",
        completion.content
    );
    assert!(completion.content.ends_with("# Cached issue\n"));
    release.send(()).unwrap();
}

pub(crate) fn uncached_running_read() -> (
    GithubReadStart,
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
) {
    let (fetcher, release, started) = running_fetch();
    let engine = GithubReadEngine::new(
        Arc::new(MemoryCache::default()),
        fetcher,
        Arc::new(CountingDownloader::default()),
        Arc::new(FixtureClock::new(2_000)),
    );
    let start = engine
        .start(
            &enabled_gh_read(),
            request(None),
            GithubReadSelector::default(),
        )
        .unwrap();
    (start, release, started)
}

#[test]
fn a_read_still_running_at_the_deadline_without_a_cached_copy_is_pending() {
    let (start, release, _started) = uncached_running_read();
    // The call budget, less the reply margin, bounds the wait.
    let _call = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(
        std::time::Duration::from_millis(1_300),
    )));
    let started = std::time::Instant::now();
    let error = crate::commands::read::wait_for_github_read(start)
        .expect_err("no copy can stand in for the running read");
    // The wait ends one reply margin before the 1.3 s call budget.
    assert!(started.elapsed() < std::time::Duration::from_millis(1_300));
    assert_eq!(error.code(), "github_read_pending", "{error}");
    assert!(
        error.to_string().contains("retry this call shortly"),
        "{error}"
    );
    release.send(()).unwrap();
}

#[cfg(test)]
use aft_engine::test_bridge::search_misc::GithubReadDeferredTestExt as _;
#[cfg(test)]
use aft_engine::test_bridge::search_misc::GithubReadEngineTestExt as _;
