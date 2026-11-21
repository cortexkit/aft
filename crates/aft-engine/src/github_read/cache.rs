use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::config::GithubConfig;
use crate::db::github_read_cache::{
    ensure_github_read_cache_schema, invalidate_github_read_cache_resource_after_schema,
    lookup_github_read_cache_entry_after_schema, prepare_github_read_cache_lookup,
    upsert_github_read_cache_entry_after_schema, GithubReadCacheEntry, GithubReadCacheKey,
    GithubReadResourceKind,
};

use super::attachments::{
    download_github_image_attachments, FlightImageDownloads, GithubImageAttachment,
    GithubImageDownloader,
};
use super::fetch::{GithubFetchRequest, GithubFetcher, GithubReadError};
use super::model::GithubDocument;
use super::render::{render_document_for_resource, render_outline_for_resource};
use super::resource::{parse_resource, GithubResource, GithubResourceKind, InvalidGithubResource};

/// Clock seam used to make freshness boundaries deterministic in tests.
pub trait GithubReadClock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// Production wall clock for cache timestamps.
#[derive(Default)]
pub struct SystemGithubReadClock;

impl GithubReadClock for SystemGithubReadClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(i64::MAX)
    }
}

/// A cache persistence seam. The production implementation stores canonical
/// text in AFT's existing `aft.db`; tests can use an in-memory fixture store.
pub trait GithubReadCacheStore: Send + Sync {
    fn lookup(&self, key: &GithubReadCacheKey) -> Result<Option<GithubReadCacheEntry>, String>;
    fn upsert(
        &self,
        key: &GithubReadCacheKey,
        canonical_text: &str,
        fetched_at_ms: i64,
        document_gap: bool,
    ) -> Result<(), String>;
    fn invalidate(
        &self,
        kind: GithubResourceKind,
        repository: &str,
        number: u64,
        authentication_identity: Option<&str>,
    ) -> Result<usize, String>;

    /// Point the store at the caller's resident `aft.db` connection, if it
    /// keeps one. Stores without a database ignore this.
    fn attach_resident_connection(&self, _connection: Option<&SharedConnection>) {}
}

/// The context's long-lived `aft.db` connection.
pub type SharedConnection = Arc<std::sync::Mutex<crate::db::TrackedConnection>>;

/// `aft.db` implementation of the GitHub read cache seam.
///
/// It works through the context's resident connection when one is attached
/// and still alive, creating the cache table once per connection. Opening the
/// database per operation (a schema peek, PRAGMAs and the migration check on
/// the request thread) is only the fallback for a caller with no resident
/// connection.
pub struct SqliteGithubReadCacheStore {
    database_path: PathBuf,
    /// Weak, so the store never keeps a connection open after the context
    /// has let it go.
    resident: Mutex<std::sync::Weak<std::sync::Mutex<crate::db::TrackedConnection>>>,
    /// Resident connection address and whether its cache has gap metadata.
    /// Cleared if a cache statement fails so the next operation checks again.
    schema_ready_on: Mutex<Option<(usize, bool)>>,
}

impl SqliteGithubReadCacheStore {
    pub fn new(database_path: impl Into<PathBuf>) -> Self {
        Self {
            database_path: database_path.into(),
            resident: Mutex::new(std::sync::Weak::new()),
            schema_ready_on: Mutex::new(None),
        }
    }

    fn connection(&self) -> Result<crate::db::TrackedConnection, String> {
        crate::db::open(&self.database_path)
            .inspect(|connection| connection.close_in_background())
            .map_err(|error| format!("failed to open GitHub read cache: {error}"))
    }

    /// Run `operation` on the resident connection, or on a fresh one when
    /// there is none. `what` names the operation in the error.
    fn run<T>(
        &self,
        what: &str,
        needs_gap_column: bool,
        operation: impl FnOnce(&rusqlite::Connection, bool) -> rusqlite::Result<T>,
    ) -> Result<T, String> {
        let failed =
            |error: rusqlite::Error| format!("failed to {what} GitHub read cache: {error}");
        let prepare = |connection: &rusqlite::Connection| {
            if needs_gap_column {
                ensure_github_read_cache_schema(connection).map(|()| true)
            } else {
                prepare_github_read_cache_lookup(connection)
            }
        };
        let resident = self.resident.lock().upgrade();
        let Some(shared) = resident else {
            let connection = self.connection()?;
            let has_gap_column = prepare(&connection).map_err(failed)?;
            return operation(&connection, has_gap_column).map_err(failed);
        };
        let connection = shared
            .lock()
            .map_err(|_| format!("failed to {what} GitHub read cache: connection lock poisoned"))?;
        let address = Arc::as_ptr(&shared) as usize;
        let mut ready = self.schema_ready_on.lock();
        let has_gap_column = match *ready {
            Some((prepared_on, has_gap_column))
                if prepared_on == address && (has_gap_column || !needs_gap_column) =>
            {
                has_gap_column
            }
            _ => {
                let has_gap_column = prepare(&connection).map_err(failed)?;
                *ready = Some((address, has_gap_column));
                has_gap_column
            }
        };
        operation(&connection, has_gap_column).map_err(|error| {
            *ready = None;
            failed(error)
        })
    }
}

impl GithubReadCacheStore for SqliteGithubReadCacheStore {
    fn lookup(&self, key: &GithubReadCacheKey) -> Result<Option<GithubReadCacheEntry>, String> {
        self.run("look up", false, |connection, has_gap_column| {
            lookup_github_read_cache_entry_after_schema(connection, key, has_gap_column)
        })
    }

    fn upsert(
        &self,
        key: &GithubReadCacheKey,
        canonical_text: &str,
        fetched_at_ms: i64,
        document_gap: bool,
    ) -> Result<(), String> {
        self.run("update", true, |connection, _| {
            upsert_github_read_cache_entry_after_schema(
                connection,
                key,
                canonical_text,
                fetched_at_ms,
                document_gap,
            )
        })
    }

    fn invalidate(
        &self,
        kind: GithubResourceKind,
        repository: &str,
        number: u64,
        authentication_identity: Option<&str>,
    ) -> Result<usize, String> {
        let number = i64::try_from(number)
            .map_err(|_| "GitHub resource number exceeds cache storage range".to_string())?;
        self.run("invalidate", true, |connection, _| {
            invalidate_github_read_cache_resource_after_schema(
                connection,
                database_kind(kind),
                repository,
                number,
                authentication_identity,
            )
        })
    }

    fn attach_resident_connection(&self, connection: Option<&SharedConnection>) {
        *self.resident.lock() = connection.map(Arc::downgrade).unwrap_or_default();
    }
}

/// Request context that the read integration supplies before entering a
/// deferred fetch. An absent capability is intentionally different from an
/// inferred one: only `Some(true)` permits image downloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubReadRequest {
    pub resource: GithubResource,
    pub working_directory: PathBuf,
    pub effective_authentication_identity: String,
    pub vision_capability: Option<bool>,
}

impl GithubReadRequest {
    pub fn parse(
        resource: &str,
        working_directory: impl Into<PathBuf>,
        effective_authentication_identity: impl Into<String>,
        vision_capability: Option<bool>,
    ) -> Result<Self, InvalidGithubResource> {
        Ok(Self {
            resource: parse_resource(resource)?,
            working_directory: working_directory.into(),
            effective_authentication_identity: effective_authentication_identity.into(),
            vision_capability,
        })
    }
}

/// Selector applied only after the complete canonical document is rendered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GithubReadSelector {
    WholeDocument,
    LineRange {
        start_line: usize,
        end_line: Option<usize>,
        limit: usize,
    },
    ByteOffset {
        offset: usize,
        limit: Option<usize>,
    },
}

impl Default for GithubReadSelector {
    fn default() -> Self {
        Self::WholeDocument
    }
}

/// Origin of the text that satisfied a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GithubReadFreshness {
    /// The response came from the live GitHub fetch.
    Fetched,
    /// The live fetch failed, so the response is the explicitly disclosed fallback copy.
    CachedFallback,
}

/// Presentation selected after the shared GitHub gate and live-fetch path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GithubReadView {
    Document,
    Outline,
}

impl GithubReadFreshness {
    /// Fallback status is part of the rendered text, where every agent can see it.
    pub const fn note(self) -> Option<&'static str> {
        None
    }
}

/// A completed GitHub read ready for the transport-specific response adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubReadCompletion {
    pub content: String,
    pub total_lines: usize,
    pub freshness: GithubReadFreshness,
    pub attachments: Vec<GithubImageAttachment>,
    /// Live structured data retained for mutation flows that must reuse read ordinals.
    pub document: Option<GithubDocument>,
    pub diff_page: Option<super::diff::GithubDiffPage>,
    /// The page stopped at [`MAX_PAGE_BYTES`]; its last line is the
    /// truncation notice, not document text.
    pub page_cut_by_size: bool,
    /// The fetch left part of the resource out, or the cache cannot establish
    /// that the fetch was complete.
    pub document_gap: bool,
}

/// Handle for a fetch or attachment task that is running away from the request
/// loop. Poll it from `PendingResponse`; never wait on it in standalone input
/// handling.
pub struct GithubReadDeferred {
    receiver: crate::pending_response::PendingResponseReceiver<
        Result<GithubReadCompletion, GithubReadError>,
    >,
    late: GithubReadLateAnswer,
}

/// The answer for a caller who stops waiting before the live fetch finishes:
/// the cached copy, with the caller's selection and labelled as a copy, or
/// [`GithubReadError::Pending`] when there is none. The fetch itself keeps
/// running and stores its result in the cache.
pub struct GithubReadLateAnswer {
    fallback: Option<(GithubReadCacheEntry, GithubReadSelector, GithubResourceKind)>,
}

impl GithubReadLateAnswer {
    /// `waited` is how long the caller waited, for the label.
    pub fn answer(
        &self,
        waited: std::time::Duration,
    ) -> Result<GithubReadCompletion, GithubReadError> {
        let still_running = format!(
            "GitHub did not answer within {}s; the read continues in the background",
            waited.as_secs_f64().ceil() as u64
        );
        match &self.fallback {
            Some((entry, selector, kind)) => Ok(complete(
                cached_fallback_text(entry, &GithubReadError::FetchFailed(still_running), *kind),
                selector.clone(),
                Vec::new(),
                DocumentSource::Cached(entry),
            )),
            None => Err(GithubReadError::Pending(format!(
                "{still_running} and its result will be cached; retry this call shortly"
            ))),
        }
    }
}

impl GithubReadDeferred {
    #[cfg(test)]
    pub(crate) fn late_answer_for_test(&self) -> &GithubReadLateAnswer {
        &self.late
    }

    /// Split the handle for a caller that polls the result channel itself and
    /// gives the late answer when its own deadline passes.
    #[doc(hidden)]
    pub fn into_parts(
        self,
    ) -> (
        crate::pending_response::PendingResponseReceiver<
            Result<GithubReadCompletion, GithubReadError>,
        >,
        GithubReadLateAnswer,
    ) {
        (self.receiver, self.late)
    }

    pub fn try_complete(&self) -> Option<Result<GithubReadCompletion, GithubReadError>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(worker_stopped())),
        }
    }

    /// Wait for the read until `deadline`. If the live fetch is still running
    /// then, give the late answer (see [`GithubReadLateAnswer`]).
    pub fn wait_until(
        &self,
        deadline: std::time::Instant,
    ) -> Result<GithubReadCompletion, GithubReadError> {
        let waited = deadline.saturating_duration_since(std::time::Instant::now());
        match self.receiver.recv_timeout(waited) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(worker_stopped()),
            Err(mpsc::RecvTimeoutError::Timeout) => self.late.answer(waited),
        }
    }
}

#[doc(hidden)]
pub fn worker_stopped() -> GithubReadError {
    GithubReadError::FetchFailed("GitHub read worker stopped before completing".to_string())
}

/// The initial outcome for a GitHub read. Every read is deferred because it
/// performs a live GitHub fetch before considering any cached fallback.
pub enum GithubReadStart {
    Immediate(GithubReadCompletion),
    Deferred(GithubReadDeferred),
}

#[derive(Default)]
struct GithubReadEngineState {
    aliases: BTreeMap<ShortResourceAlias, String>,
    flights: BTreeMap<GithubReadFlightSlot, Vec<GithubReadFlightWaiter>>,
}

/// Which repository a shorthand (`issue://42`) read in one checkout resolved
/// to last. Repository resolution depends on the checkout, not on who reads,
/// so the key carries no identity and the map grows only with distinct
/// resources read, not with sessions.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ShortResourceAlias {
    kind: GithubResourceKind,
    number: u64,
    working_directory: PathBuf,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CacheSlot {
    kind: GithubResourceKind,
    repository: String,
    number: u64,
    authentication_identity_hash: [u8; 32],
}

/// A resolved resource can share a flight across equivalent explicit forms.
/// A shorthand read stays in its own flight, keyed by checkout and reader,
/// because the repository it names is only known once `gh` answers: the
/// last-seen alias may be stale (the checkout's default repository changed).
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum GithubReadFlightSlot {
    Resolved(CacheSlot),
    Unresolved(ShortResourceAlias, [u8; 32]),
}

struct GithubReadFlightWaiter {
    request: GithubReadRequest,
    selector: GithubReadSelector,
    view: GithubReadView,
    fallback: Option<GithubReadCacheEntry>,
    sender: crate::pending_response::PendingResponseSender<
        Result<GithubReadCompletion, GithubReadError>,
    >,
}

/// Coordinates live GitHub fetches, durable fallback copies, single-flight work,
/// rendering, and deferred attachments for both issue and PR reads.
pub struct GithubReadEngine {
    cache: Arc<dyn GithubReadCacheStore>,
    fetcher: Arc<dyn GithubFetcher>,
    image_downloader: Arc<dyn GithubImageDownloader>,
    clock: Arc<dyn GithubReadClock>,
    state: Arc<Mutex<GithubReadEngineState>>,
}

impl GithubReadEngine {
    #[cfg(test)]
    pub(crate) fn flights_empty_for_test(&self) -> bool {
        self.state.lock().flights.is_empty()
    }

    pub fn new(
        cache: Arc<dyn GithubReadCacheStore>,
        fetcher: Arc<dyn GithubFetcher>,
        image_downloader: Arc<dyn GithubImageDownloader>,
        clock: Arc<dyn GithubReadClock>,
    ) -> Self {
        Self {
            cache,
            fetcher,
            image_downloader,
            clock,
            state: Arc::new(Mutex::new(GithubReadEngineState::default())),
        }
    }

    /// Begin a resource-string read with strict parser validation.
    pub fn start_resource(
        &self,
        github: &GithubConfig,
        resource: &str,
        working_directory: impl Into<PathBuf>,
        effective_authentication_identity: impl Into<String>,
        vision_capability: Option<bool>,
        selector: GithubReadSelector,
    ) -> Result<GithubReadStart, GithubReadError> {
        self.start_resource_with_view(
            github,
            resource,
            working_directory,
            effective_authentication_identity,
            vision_capability,
            selector,
            GithubReadView::Document,
        )
    }

    /// Begin an alternate read-only view with the same typed gate, live fetch,
    /// and durable fallback behavior as `read`.
    pub fn start_resource_with_view(
        &self,
        github: &GithubConfig,
        resource: &str,
        working_directory: impl Into<PathBuf>,
        effective_authentication_identity: impl Into<String>,
        vision_capability: Option<bool>,
        selector: GithubReadSelector,
        view: GithubReadView,
    ) -> Result<GithubReadStart, GithubReadError> {
        self.require_enabled(github)?;
        let request = GithubReadRequest::parse(
            resource,
            working_directory,
            effective_authentication_identity,
            vision_capability,
        )
        .map_err(|error| GithubReadError::invalid_resource(error.to_string()))?;
        self.start_with_view(github, request, selector, view)
    }

    /// Start a read without blocking on GitHub or an image host.
    pub fn start(
        &self,
        github: &GithubConfig,
        request: GithubReadRequest,
        selector: GithubReadSelector,
    ) -> Result<GithubReadStart, GithubReadError> {
        self.start_with_view(github, request, selector, GithubReadView::Document)
    }

    pub fn start_with_view(
        &self,
        github: &GithubConfig,
        request: GithubReadRequest,
        selector: GithubReadSelector,
        view: GithubReadView,
    ) -> Result<GithubReadStart, GithubReadError> {
        self.require_enabled(github)?;
        if request.resource.diff_path.is_some() {
            if view != GithubReadView::Document {
                return Err(GithubReadError::invalid_resource(
                    "Use read for diffs; aft_outline and aft_zoom do not support diff links",
                ));
            }
            // Diff requests bypass cache lookup and shared document flights.
            // Every page must reflect a fresh PR head, even after a failed fetch.
            // The completion wake is captured here, so a quiet connection is
            // still told when the diff arrives.
            let (sender, receiver) = crate::pending_response::pending_response_channel();
            let fetcher = Arc::clone(&self.fetcher);
            std::thread::spawn(move || {
                let result = fetcher
                    .fetch_diff(&GithubFetchRequest {
                        resource: request.resource,
                        working_directory: request.working_directory,
                    })
                    .map(|diff| diff.page(selector));
                let _ = sender.send(result);
            });
            return Ok(GithubReadStart::Deferred(GithubReadDeferred {
                receiver,
                late: GithubReadLateAnswer { fallback: None },
            }));
        }
        let fallback = self.cache_fallback_for_request(&request);
        Ok(self.defer_fetch(request, selector, view, fallback))
    }

    /// Invalidate the exact resource after a successful structured `gh` mutation.
    /// Passing no identity conservatively removes every principal's cache row.
    pub fn invalidate(
        &self,
        kind: GithubResourceKind,
        resolved_repository: &str,
        number: u64,
        effective_authentication_identity: Option<&str>,
    ) -> Result<usize, GithubReadError> {
        self.cache
            .invalidate(
                kind,
                resolved_repository,
                number,
                effective_authentication_identity,
            )
            .map_err(cache_failure)
    }

    /// Let the cache use the caller's resident `aft.db` connection instead of
    /// opening the database for each operation.
    pub fn attach_resident_connection(&self, connection: Option<&SharedConnection>) {
        self.cache.attach_resident_connection(connection);
    }

    fn require_enabled(&self, github: &GithubConfig) -> Result<(), GithubReadError> {
        if github.read {
            Ok(())
        } else {
            Err(GithubReadError::GithubReadDisabled)
        }
    }

    fn resolved_cache_slot(&self, request: &GithubReadRequest) -> Option<CacheSlot> {
        let repository = match &request.resource.repository {
            Some(repository) => Some(repository.clone()),
            None => self
                .state
                .lock()
                .aliases
                .get(&short_alias(request))
                .cloned(),
        }?;
        Some(cache_slot(
            &request.resource,
            &repository,
            &request.effective_authentication_identity,
        ))
    }

    fn cache_fallback_for_request(
        &self,
        request: &GithubReadRequest,
    ) -> Option<GithubReadCacheEntry> {
        // Cache rows contain the default compressed document. A discussion
        // drill-down promises structurally stripped full bodies, so a stale
        // default render cannot honestly stand in for that request.
        if request.resource.comment_selector.is_some() {
            return None;
        }
        let slot = self.resolved_cache_slot(request)?;
        let key = match github_cache_key(&slot, &request.effective_authentication_identity) {
            Some(key) => key,
            None => return None,
        };
        match self.cache.lookup(&key) {
            Ok(entry) => entry,
            Err(error) => {
                log::warn!("GitHub read cache lookup failed; live fetch will continue: {error}");
                None
            }
        }
    }

    fn flight_slot_for_request(&self, request: &GithubReadRequest) -> GithubReadFlightSlot {
        let identity = &request.effective_authentication_identity;
        match &request.resource.repository {
            Some(repository) => {
                GithubReadFlightSlot::Resolved(cache_slot(&request.resource, repository, identity))
            }
            None => GithubReadFlightSlot::Unresolved(
                short_alias(request),
                authentication_identity_hash(identity),
            ),
        }
    }

    fn defer_fetch(
        &self,
        request: GithubReadRequest,
        selector: GithubReadSelector,
        view: GithubReadView,
        fallback: Option<GithubReadCacheEntry>,
    ) -> GithubReadStart {
        let slot = self.flight_slot_for_request(&request);
        let fetch_request = request.clone();
        let timeout_fallback = fallback
            .clone()
            .map(|entry| (entry, selector.clone(), request.resource.kind));
        // Every coalesced caller owns its own wake-bound sender. Capturing only
        // the leader's connection would leave followers waiting on idle routes.
        let (sender, receiver) = crate::pending_response::pending_response_channel();
        let leader = {
            let mut state = self.state.lock();
            let waiters = state.flights.entry(slot.clone()).or_default();
            let leader = waiters.is_empty();
            waiters.push(GithubReadFlightWaiter {
                request,
                selector,
                view,
                fallback,
                sender,
            });
            leader
        };
        if leader {
            let cache = Arc::clone(&self.cache);
            let fetcher = Arc::clone(&self.fetcher);
            let downloader = Arc::clone(&self.image_downloader);
            let clock = Arc::clone(&self.clock);
            let state = Arc::clone(&self.state);
            std::thread::spawn(move || {
                // A failed producer must finish its waiters and release the slot
                // so a later read can fetch again instead of joining a dead flight.
                let fetched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    fetch_store(
                        &fetch_request,
                        cache.as_ref(),
                        fetcher.as_ref(),
                        clock.as_ref(),
                        &state,
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(GithubReadError::FetchFailed(
                        "GitHub read worker stopped before completing".to_string(),
                    ))
                });
                let waiters = state.lock().flights.remove(&slot).unwrap_or_default();
                // Coalesced readers share one download of each image.
                let downloads = FlightImageDownloads::new(downloader.as_ref());
                for waiter in waiters {
                    let result = match &fetched {
                        Ok(document) => {
                            render_for_view(document, &waiter.request.resource, waiter.view)
                                .and_then(|canonical_text| {
                                    complete_with_optional_attachments(
                                        &waiter.request,
                                        waiter.selector,
                                        canonical_text,
                                        &downloads,
                                        waiter.view,
                                        DocumentSource::Fetched(document),
                                    )
                                })
                        }
                        Err(error) => match waiter.fallback {
                            Some(entry) => complete_with_optional_attachments(
                                &waiter.request,
                                waiter.selector,
                                cached_fallback_text(&entry, error, waiter.request.resource.kind),
                                &downloads,
                                waiter.view,
                                DocumentSource::Cached(&entry),
                            ),
                            None => Err(error.clone()),
                        },
                    };
                    let _ = waiter.sender.send(result);
                }
            });
        }
        GithubReadStart::Deferred(GithubReadDeferred {
            receiver,
            late: GithubReadLateAnswer {
                fallback: timeout_fallback,
            },
        })
    }
}

fn fetch_store(
    request: &GithubReadRequest,
    cache: &dyn GithubReadCacheStore,
    fetcher: &dyn GithubFetcher,
    clock: &dyn GithubReadClock,
    state: &Mutex<GithubReadEngineState>,
) -> Result<GithubDocument, GithubReadError> {
    let document = fetcher.fetch(&GithubFetchRequest {
        resource: request.resource.clone(),
        working_directory: request.working_directory.clone(),
    })?;
    let repository = document.repository.clone();
    let cache_resource = super::resource::GithubResource {
        kind: request.resource.kind,
        number: request.resource.number,
        repository: Some(repository.clone()),
        comment_selector: None,
        diff_path: None,
    };
    let canonical_text = render_document_for_resource(&document, &cache_resource)
        .expect("cache rendering has no discussion selector");
    let slot = cache_slot(
        &request.resource,
        &repository,
        &request.effective_authentication_identity,
    );
    // A cache outage or an unrepresentable cache key must not convert a live
    // GitHub document into a failed read. The next request will fetch live again.
    if let Some(key) = github_cache_key(&slot, &request.effective_authentication_identity) {
        if let Err(error) = cache.upsert(
            &key,
            &canonical_text,
            clock.now_ms(),
            document.has_fetch_gap(),
        ) {
            log::warn!("GitHub read cache write failed: {error}");
        }
    }
    if request.resource.repository.is_none() {
        state
            .lock()
            .aliases
            .insert(short_alias(request), repository);
    }
    Ok(document)
}

fn render_for_view(
    document: &GithubDocument,
    resource: &GithubResource,
    view: GithubReadView,
) -> Result<String, GithubReadError> {
    match view {
        GithubReadView::Document => render_document_for_resource(document, resource),
        GithubReadView::Outline => Ok(render_outline_for_resource(document, resource)),
    }
}

enum DocumentSource<'a> {
    Fetched(&'a GithubDocument),
    Cached(&'a GithubReadCacheEntry),
}

fn complete_with_optional_attachments(
    request: &GithubReadRequest,
    selector: GithubReadSelector,
    canonical_text: String,
    downloader: &dyn GithubImageDownloader,
    view: GithubReadView,
    source: DocumentSource<'_>,
) -> Result<GithubReadCompletion, GithubReadError> {
    let attachments = if view == GithubReadView::Document && request.vision_capability == Some(true)
    {
        download_github_image_attachments(&canonical_text, downloader)
    } else {
        Vec::new()
    };
    Ok(complete(canonical_text, selector, attachments, source))
}

fn complete(
    canonical_text: String,
    selector: GithubReadSelector,
    attachments: Vec<GithubImageAttachment>,
    source: DocumentSource<'_>,
) -> GithubReadCompletion {
    let total_lines = canonical_text.lines().count();
    let (content, page_cut_by_size) = select(&canonical_text, selector);
    let (freshness, document, document_gap) = match source {
        DocumentSource::Fetched(document) => (
            GithubReadFreshness::Fetched,
            Some(document.clone()),
            document.has_fetch_gap(),
        ),
        DocumentSource::Cached(entry) => (
            GithubReadFreshness::CachedFallback,
            None,
            entry.document_gap.unwrap_or(true),
        ),
    };
    GithubReadCompletion {
        content,
        total_lines,
        freshness,
        attachments,
        document,
        diff_page: None,
        page_cut_by_size,
        document_gap,
    }
}

/// Longest line a page shows whole, and most bytes one page holds: the same
/// bounds file reads apply (`commands/read.rs`).
const MAX_LINE_LENGTH: usize = 2000;
const MAX_PAGE_BYTES: usize = 50 * 1024;

/// Apply selection to the completed canonical render, never to raw GitHub data.
pub fn apply_selector(canonical_text: &str, selector: GithubReadSelector) -> String {
    select(canonical_text, selector).0
}

/// [`apply_selector`], also returning whether a line page stopped at
/// [`MAX_PAGE_BYTES`].
fn select(canonical_text: &str, selector: GithubReadSelector) -> (String, bool) {
    match selector {
        GithubReadSelector::WholeDocument => (canonical_text.to_string(), false),
        GithubReadSelector::LineRange {
            start_line,
            end_line,
            limit,
        } => {
            let start_index = start_line.saturating_sub(1);
            let requested_end =
                end_line.unwrap_or_else(|| start_line.saturating_add(limit).saturating_sub(1));
            let mut page = String::new();
            for line in canonical_text
                .lines()
                .skip(start_index)
                .take(requested_end.saturating_sub(start_index))
            {
                let shown = if line.len() > MAX_LINE_LENGTH {
                    let safe_end = line.floor_char_boundary(MAX_LINE_LENGTH);
                    format!("{}... (truncated)\n", &line[..safe_end])
                } else {
                    format!("{line}\n")
                };
                if page.len().saturating_add(shown.len()) > MAX_PAGE_BYTES {
                    page.push_str(&format!(
                        "... (output truncated at {}KB, use start_line/end_line to read sections)\n",
                        MAX_PAGE_BYTES / 1024
                    ));
                    return (page, true);
                }
                page.push_str(&shown);
            }
            (page, false)
        }
        GithubReadSelector::ByteOffset { offset, limit } => {
            let start = canonical_text.floor_char_boundary(offset.min(canonical_text.len()));
            let requested_end = limit
                .map(|limit| start.saturating_add(limit).min(canonical_text.len()))
                .unwrap_or(canonical_text.len());
            let end = canonical_text.floor_char_boundary(requested_end);
            (canonical_text[start..end].to_string(), false)
        }
    }
}

fn short_alias(request: &GithubReadRequest) -> ShortResourceAlias {
    ShortResourceAlias {
        kind: request.resource.kind,
        number: request.resource.number,
        working_directory: request.working_directory.clone(),
    }
}

fn cache_slot(
    resource: &GithubResource,
    repository: &str,
    authentication_identity: &str,
) -> CacheSlot {
    CacheSlot {
        kind: resource.kind,
        repository: repository.trim().to_ascii_lowercase(),
        number: resource.number,
        authentication_identity_hash: authentication_identity_hash(authentication_identity),
    }
}

fn github_cache_key(slot: &CacheSlot, authentication_identity: &str) -> Option<GithubReadCacheKey> {
    let number = match i64::try_from(slot.number) {
        Ok(number) => number,
        Err(_) => {
            log::warn!(
                "GitHub resource number exceeds cache storage range; fallback is unavailable"
            );
            return None;
        }
    };
    Some(GithubReadCacheKey::new(
        database_kind(slot.kind),
        &slot.repository,
        number,
        authentication_identity,
    ))
}

fn database_kind(kind: GithubResourceKind) -> GithubReadResourceKind {
    match kind {
        GithubResourceKind::Issue => GithubReadResourceKind::Issue,
        GithubResourceKind::PullRequest => GithubReadResourceKind::PullRequest,
    }
}

fn authentication_identity_hash(identity: &str) -> [u8; 32] {
    *blake3::hash(identity.as_bytes()).as_bytes()
}

fn cache_failure(error: String) -> GithubReadError {
    GithubReadError::FetchFailed(format!("GitHub read cache is unavailable: {error}"))
}

fn cached_fallback_text(
    entry: &GithubReadCacheEntry,
    error: &GithubReadError,
    kind: GithubResourceKind,
) -> String {
    let thread_warning = if kind == GithubResourceKind::PullRequest {
        super::thread_text::STALE
    } else {
        ""
    };
    format!(
        "[cached copy from {}; live fetch failed: {}]\n{thread_warning}{}",
        iso8601_utc(entry.fetched_at_ms),
        short_failure_reason(error),
        entry.canonical_text
    )
}

fn short_failure_reason(error: &GithubReadError) -> String {
    const MAX_REASON_CHARS: usize = 160;
    let reason = error
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut characters = reason.chars();
    let mut short = characters
        .by_ref()
        .take(MAX_REASON_CHARS)
        .collect::<String>();
    if characters.next().is_some() {
        short.push('…');
    }
    if short.is_empty() {
        "unknown fetch error".to_string()
    } else {
        short
    }
}

fn iso8601_utc(unix_ms: i64) -> String {
    let seconds = unix_ms.div_euclid(1_000);
    let milliseconds = unix_ms.rem_euclid(1_000);
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_date_from_unix_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milliseconds:03}Z")
}

fn civil_date_from_unix_days(days: i64) -> (i64, i64, i64) {
    // Convert epoch days to a civil UTC date inline to avoid adding a second
    // wall-clock dependency just to format fallback timestamps.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// Convenience constructor for the real cache location. The read integration
/// supplies its existing `aft.db` path; this module never creates a second DB.
pub fn sqlite_cache_store(database_path: impl AsRef<Path>) -> Arc<dyn GithubReadCacheStore> {
    Arc::new(SqliteGithubReadCacheStore::new(database_path.as_ref()))
}

#[cfg(test)]
mod private_tests;
