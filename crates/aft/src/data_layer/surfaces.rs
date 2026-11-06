//! The data layer on AFT's status surfaces.
//!
//! With the data layer on, the status tool and its pushes, daemon health,
//! the memory census, zoom's call-graph status, and the `aft warmup` and
//! `aft index` commands describe the root's checkout in the data layer for
//! the planes it serves: text (grep and glob) and the call graph. The status
//! tool's `semantic_index` describes the checkout's meaning plane once a
//! search or a configure has given it the root's semantic configuration:
//! ready only when every file it applies to is filled. With the data layer
//! off, nothing here runs and every surface reports today's stores.
//!
//! Reading status never binds a checkout, since binding starts a build: a
//! root that nothing has queried yet reports its planes off, with the
//! reason `not_bound`, and counts as settled.
//!
//! The runtime resolves recorded edits into the call graph lazily, so a
//! published call graph with edits waiting is the normal state: it reports
//! ready, with the waiting edits counted, never building.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use super::checkout::{
    refusal_message, CheckoutMemory, CheckoutStatus, CheckoutView, DataError, EngineMemory,
    PlaneStatus, INDEX_STORE,
};
use super::runtime::{DataCheckout, DataFailure};
use crate::context::{AppContext, SemanticFillHealth, SemanticHealthComponentSnapshot};
use crate::memory::MemoryEstimate;
use aft_store::family::NewerFormat;

/// A plane's state on the status surfaces, in the words the consumers of
/// `feature_status` already use.
pub(crate) const OFF: &str = "off";
pub(crate) const BUILDING: &str = "building";
pub(crate) const READY: &str = "ready";
pub(crate) const UNAVAILABLE: &str = "unavailable";

/// Why a plane is off or unavailable.
pub(crate) mod reason {
    /// Nothing has queried the root yet, so its checkout is not bound and
    /// nothing is built.
    pub(crate) const NOT_BOUND: &str = "not_bound";
    /// The family store could not be opened for the checkout.
    pub(crate) const BIND_FAILED: &str = "bind_failed";
    /// The checkout's build failed.
    pub(crate) const BUILD_FAILED: &str = "build_failed";
    /// Resolving the edits recorded since the publication failed.
    pub(crate) const REFRESH_FAILED: &str = "refresh_failed";
    /// Another process held the family store's writer past the wait.
    pub(crate) const STORE_BUSY: &str = "store_busy";
    /// The store's disk has less free space than AFT's floor, so nothing
    /// new is written; the error is the approved pause sentence.
    pub(crate) const DISK_RESERVE: &str = "disk_reserve";
    /// A newer build wrote a file of the family store in a format version
    /// above what this build reads, so the store is refused; the error is
    /// the refusal message. The same code today's stores use for theirs.
    pub(crate) const REQUIRES_NEWER_READER: &str = crate::persisted_format::CODE;
    /// Configure leaves the root unindexed: it is too broad to index (a
    /// filesystem or volume root), or it is the home folder.
    pub(crate) const ROOT_TOO_BROAD: &str = crate::feature_status::cause::ROOT_TOO_BROAD;
    pub(crate) const HOME_ROOT: &str = crate::feature_status::cause::HOME_ROOT;
}

/// This root's checkout in the data layer, as a status read finds it.
pub(crate) enum DataCheckoutProbe {
    /// Nothing has bound it.
    NotBound,
    /// A bind is in progress (it scans the checkout), or the handle's slot
    /// is briefly held.
    Binding,
    /// The last bind failed, with the error.
    BindFailed(DataError),
    /// The root is never bound; see [`Withheld`].
    Withheld(Withheld),
    Bound(Arc<DataCheckout>),
}

/// Why a root's checkout is never bound in the data layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Withheld {
    /// The root is a filesystem, volume or drive root.
    TooBroad,
    /// The root is the home folder.
    Home,
    /// The text and call-graph indexes are both switched off.
    IndexesOff,
}

/// One plane as status, health and zoom report it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlaneReport {
    /// [`OFF`], [`BUILDING`], [`READY`] or [`UNAVAILABLE`].
    pub(crate) status: &'static str,
    /// Why the plane is off or unavailable; see [`reason`].
    pub(crate) reason: Option<&'static str>,
    /// The failure's error text, when a failure made it unavailable.
    pub(crate) error: Option<String>,
}

impl PlaneReport {
    const fn of(status: &'static str) -> Self {
        Self {
            status,
            reason: None,
            error: None,
        }
    }

    fn because(status: &'static str, reason: &'static str, error: Option<String>) -> Self {
        Self {
            status,
            reason: Some(reason),
            error,
        }
    }

    fn failed(failure: &DataFailure) -> Self {
        let reason = if failure.refusal.is_some() {
            reason::REQUIRES_NEWER_READER
        } else if crate::view_storage_reserve::is_message(&failure.error) {
            reason::DISK_RESERVE
        } else if failure.busy {
            reason::STORE_BUSY
        } else if failure.stage == "refresh" {
            reason::REFRESH_FAILED
        } else {
            reason::BUILD_FAILED
        };
        Self::because(UNAVAILABLE, reason, Some(failure.error.clone()))
    }

    /// `{status, reason?, error?}`.
    fn to_json(&self) -> Value {
        let mut value = json!({ "status": self.status });
        if let Some(reason) = self.reason {
            value["reason"] = json!(reason);
        }
        if let Some(error) = &self.error {
            value["error"] = json!(error);
        }
        value
    }
}

/// How a root's checkout stands in the data layer, for every status
/// surface.
#[derive(Clone, Debug)]
pub(crate) struct DataStoreReport {
    pub(crate) text: PlaneReport,
    pub(crate) callgraph: PlaneReport,
    /// Paths of the checkout as last published; `None` before anything is.
    pub(crate) files: Option<usize>,
    /// While the text plane is building over a publication: paths whose
    /// text it has indexed.
    pub(crate) indexed_files: Option<usize>,
    /// Edits recorded since the publication that the call graph has not
    /// resolved yet. The graph answers meanwhile; an exact reader waits for
    /// them.
    pub(crate) pending_edits: usize,
    /// Files in a parsed language that the call graph does not cover,
    /// because extracting them failed.
    pub(crate) unparsed_files: usize,
    /// The last build or refresh that failed, until one succeeds; kept even
    /// while an older publication still serves.
    pub(crate) failure: Option<DataFailure>,
    /// The family-store file a newer build wrote, when the last bind, build
    /// or refresh refused it; status lists it among its storage refusals.
    pub(crate) refusal: Option<NewerFormat>,
    /// The family store's folder, for its disk use.
    pub(crate) family_dir: Option<PathBuf>,
    /// Meaning search's plane: the status tool's and each root's health
    /// `semantic_index`. `None` while semantic search is off for the
    /// checkout, or before anything gave it the root's semantic
    /// configuration; today's index then reports.
    pub(crate) meaning: Option<MeaningReport>,
}

impl DataStoreReport {
    fn both(plane: PlaneReport) -> Self {
        Self {
            text: plane.clone(),
            callgraph: plane,
            files: None,
            indexed_files: None,
            pending_edits: 0,
            unparsed_files: 0,
            failure: None,
            refusal: None,
            family_dir: None,
            meaning: None,
        }
    }

    /// The status tool's `search_index` object.
    pub(crate) fn search_index_json(&self) -> Value {
        let mut value = self.text.to_json();
        if let Some(files) = self.files {
            value["files"] = json!(files);
        }
        if let Some(indexed) = self.indexed_files {
            value["indexed_files"] = json!(indexed);
        }
        value
    }

    /// The status tool's `callgraph_store` object.
    pub(crate) fn callgraph_store_json(&self) -> Value {
        let mut value = self.callgraph.to_json();
        value["pending_edits"] = json!(self.pending_edits);
        value["unparsed_files"] = json!(self.unparsed_files);
        value
    }

    /// The `data_store` object of status and of each root's health.
    pub(crate) fn health(&self) -> DataStoreHealth {
        let plane_reason = [&self.text, &self.callgraph]
            .into_iter()
            .find(|plane| plane.reason.is_some());
        DataStoreHealth {
            reason: plane_reason.and_then(|plane| plane.reason),
            error: plane_reason.and_then(|plane| plane.error.clone()),
            pending_edits: self.pending_edits,
            unparsed_files: self.unparsed_files,
            failure: self.failure.as_ref().map(|failure| DataStoreFailure {
                stage: failure.stage,
                error: failure.error.clone(),
                busy: failure.busy,
                at_ms: failure.at_ms,
            }),
        }
    }
}

/// The `data_store` object of status and of each root's health.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DataStoreHealth {
    /// Why a plane is off or unavailable; see [`reason`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub pending_edits: usize,
    pub unparsed_files: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<DataStoreFailure>,
}

/// The last failed build or refresh of a checkout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DataStoreFailure {
    /// `build` or `refresh`.
    pub stage: &'static str,
    pub error: String,
    /// Another process held the store's writer past the wait.
    pub busy: bool,
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
}

/// How `ctx`'s checkout stands in the data layer, without binding it or
/// waiting for a bind; `None` when the data layer is off.
#[cfg(test)]
thread_local! {
    /// Storage folders handed out by `serve_text_index_for_test`, removed when
    /// the test's thread ends.
    static TEST_STORAGE: std::cell::RefCell<Vec<tempfile::TempDir>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Binds `ctx`'s index-store checkout over the files already on disk and
/// waits until its text index serves them. A context with no storage folder
/// gets a throwaway one, so a test never writes into the real storage.
#[cfg(test)]
pub(crate) fn serve_text_index_for_test(ctx: &AppContext) {
    serve_text_for_test(ctx, true);
}

/// Binds the index store's checkout of `ctx`'s root and waits until its text
/// index serves, without waiting for the call graph: for a test that holds
/// the call graph's build to look at a checkout whose text alone is ready.
#[cfg(test)]
pub(crate) fn serve_text_only_for_test(ctx: &AppContext) {
    serve_text_for_test(ctx, false);
}

#[cfg(test)]
fn serve_text_for_test(ctx: &AppContext, whole_build: bool) {
    bind_for_test(ctx);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    // The text index publishes before the call graph; the build's last
    // publication changes the text generation answers are cached by, so a
    // test that compares answers across calls waits for the build to end.
    while text_plane_ready(ctx) != Some(true)
        || (whole_build
            && ctx
                .data_checkout()
                .is_some_and(|checkout| checkout.checkout().status().building))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the text index never served: {:?}",
            report(ctx).map(|report| report.text.status)
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Binds the index store's checkout of `ctx`'s root, giving the context a
/// throwaway storage folder when it has none, without waiting for a build.
#[cfg(test)]
pub(crate) fn bind_for_test(ctx: &AppContext) {
    if ctx.config().storage_dir.is_none() {
        let dir = tempfile::tempdir().expect("test storage folder");
        let path = dir.path().to_path_buf();
        TEST_STORAGE.with(|dirs| dirs.borrow_mut().push(dir));
        ctx.update_config(|config| config.storage_dir = Some(path));
    }
    if ctx.canonical_cache_root_opt().is_none() {
        if let Some(root) = ctx.config().project_root.clone() {
            ctx.set_canonical_cache_root(std::fs::canonicalize(&root).unwrap_or(root));
        }
    }
    ctx.data_checkout().expect("the index store binds the root");
}

/// Whether this root's text index serves: `Some(true)` ready, `Some(false)`
/// still building, `None` when no checkout is bound or its text plane is off.
/// For callers outside the crate, such as `aft warmup`.
pub fn text_plane_ready(ctx: &AppContext) -> Option<bool> {
    match report(ctx)?.text.status {
        READY => Some(true),
        BUILDING => Some(false),
        _ => None,
    }
}

pub(crate) fn report(ctx: &AppContext) -> Option<DataStoreReport> {
    Some(match ctx.probe_data_checkout()? {
        DataCheckoutProbe::NotBound => {
            DataStoreReport::both(PlaneReport::because(OFF, reason::NOT_BOUND, None))
        }
        DataCheckoutProbe::Binding => DataStoreReport::both(PlaneReport::of(BUILDING)),
        DataCheckoutProbe::BindFailed(error) => {
            let reason = match error.refusal() {
                Some(_) => reason::REQUIRES_NEWER_READER,
                None => reason::BIND_FAILED,
            };
            DataStoreReport {
                refusal: error.refusal().cloned(),
                ..DataStoreReport::both(PlaneReport::because(
                    UNAVAILABLE,
                    reason,
                    Some(error.to_string()),
                ))
            }
        }
        // As today's indexes report these roots.
        DataCheckoutProbe::Withheld(Withheld::TooBroad) => {
            DataStoreReport::both(PlaneReport::because(OFF, reason::ROOT_TOO_BROAD, None))
        }
        DataCheckoutProbe::Withheld(Withheld::Home) => {
            DataStoreReport::both(PlaneReport::because(UNAVAILABLE, reason::HOME_ROOT, None))
        }
        DataCheckoutProbe::Withheld(Withheld::IndexesOff) => {
            DataStoreReport::both(PlaneReport::of(OFF))
        }
        DataCheckoutProbe::Bound(checkout) => report_of(&checkout),
    })
}

/// The family-store file a newer build wrote that `ctx`'s checkout refused
/// at its last bind, build or refresh, without binding it; `None` when the
/// data layer is off or nothing was refused.
pub(crate) fn storage_refusal(ctx: &AppContext) -> Option<NewerFormat> {
    report(ctx)?.refusal
}

/// The degraded reason status gives a refused family store, in the form
/// today's stores give theirs: `storage_requires_newer_reader:index_store`.
pub(crate) fn refusal_reason() -> String {
    format!("{}:{INDEX_STORE}", crate::persisted_format::CODE)
}

/// The `storage_refusals` entry status gives a refused family-store file,
/// shaped as today's stores' entries.
pub(crate) fn refusal_json(refusal: &NewerFormat) -> Value {
    json!({
        "code": crate::persisted_format::CODE,
        "store": INDEX_STORE,
        "path": refusal.path.display().to_string(),
        "found": refusal.found,
        "supported": refusal.supported,
        "message": refusal_message(refusal),
    })
}

/// How a bound checkout stands.
pub(crate) fn report_of(checkout: &DataCheckout) -> DataStoreReport {
    let status = checkout.checkout().status();
    let files = checkout.view().map(|view| view.state().paths().len());
    let mut report = report_from(
        &status,
        files,
        checkout.last_failure(),
        Some(checkout.checkout().family_dir().to_path_buf()),
    );
    report.meaning = meaning_report(checkout);
    report
}

/// Meaning search's plane as the status tool and each root's health report
/// it: ready once every file meaning search applies to is filled; building
/// while the fill is under way (with how many files it has accounted for)
/// or before it has learned the model; failed when no fill could start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MeaningReport {
    pub(crate) plane: MeaningPlane,
    /// A fill round is queued or running.
    pub(crate) filling: bool,
    /// The running fill round waits for another fill of the repository, in
    /// this process or another, and embeds nothing until that one publishes.
    pub(crate) waiting: bool,
    /// When the fill and its last store happened, as the fill recorded
    /// them (see [`super::meaning::FillTimes`]).
    pub(crate) times: super::meaning::FillTimes,
}

/// How the meaning plane stands; see [`MeaningReport`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MeaningPlane {
    /// The plane serves the files filled so far: complete, or partial while
    /// files wait for vectors. `error` is the last fill's failure, kept until
    /// a fill succeeds.
    Serving {
        complete: bool,
        coverage: MeaningCoverage,
        error: Option<String>,
    },
    /// Nothing can be served yet; `stage` says what the fill is doing, and
    /// the counts are files once the fill has surveyed them.
    Building {
        stage: String,
        files: Option<usize>,
        entries_done: Option<usize>,
        entries_total: Option<usize>,
    },
    /// No fill could start, with why.
    Failed(String),
    Disabled,
    Ready,
}

/// The meaning plane's coverage counts (see [`super::vectors::Coverage`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MeaningCoverage {
    pub(crate) files: usize,
    pub(crate) files_embedded: usize,
    pub(crate) files_too_large: usize,
    pub(crate) files_failed: usize,
    pub(crate) files_pending: usize,
    pub(crate) files_past_cap: usize,
    pub(crate) chunks: usize,
    pub(crate) chunks_embedded: usize,
    pub(crate) chunks_failed: usize,
}

impl MeaningCoverage {
    fn of(coverage: &super::vectors::Coverage) -> Self {
        Self {
            files: coverage.files,
            files_embedded: coverage.files_embedded,
            files_too_large: coverage.files_too_large,
            files_failed: coverage.files_failed.len(),
            files_pending: coverage.pending.len(),
            files_past_cap: coverage.past_cap,
            chunks: coverage.chunks,
            chunks_embedded: coverage.chunks_embedded,
            chunks_failed: coverage.chunks_failed.len(),
        }
    }
}

impl MeaningReport {
    /// The status tool's `semantic_index` object.
    pub(crate) fn status_json(&self) -> Value {
        let mut value = match &self.plane {
            MeaningPlane::Serving {
                complete,
                coverage,
                error,
            } => {
                let mut value = json!({
                    "status": if *complete { READY } else { BUILDING },
                    "files": coverage.files,
                    "files_embedded": coverage.files_embedded,
                    "files_too_large": coverage.files_too_large,
                    "files_failed": coverage.files_failed,
                    "files_pending": coverage.files_pending,
                    "files_past_cap": coverage.files_past_cap,
                    "chunks": coverage.chunks,
                    "chunks_embedded": coverage.chunks_embedded,
                    "chunks_failed": coverage.chunks_failed,
                });
                if !complete {
                    value["stage"] = json!(super::meaning::FILLING_STAGE);
                }
                if let Some(error) = error {
                    value["error"] = json!(error);
                }
                value
            }
            MeaningPlane::Building {
                stage,
                files,
                entries_done,
                entries_total,
            } => json!({
                "status": BUILDING,
                "stage": stage,
                "files": files,
                "entries_done": entries_done,
                "entries_total": entries_total,
            }),
            MeaningPlane::Failed(error) => json!({ "status": "failed", "error": error }),
            MeaningPlane::Disabled => json!({ "status": "disabled" }),
            MeaningPlane::Ready => json!({ "status": READY }),
        };
        if self.waiting {
            value["stage"] = json!(super::meaning::WAITING_STAGE);
        }
        value["state"] = value["status"].clone();
        value["refreshing_count"] = json!(0);
        value["filling"] = json!(self.filling);
        value["waiting_for_another_fill"] = json!(self.waiting);
        value
    }

    /// Each root's health `semantic_index`, in the shape health gives
    /// today's index: `status`, `stage` (which also says when the fill waits
    /// for another fill), `reason` (the fill's failure) and
    /// `embedded_chunks`/`total_chunks`, with the fill's file counts beside
    /// them (see [`SemanticFillHealth`] for which are present when). A
    /// failed plane reports `degraded`, as health reports a failed index.
    pub(crate) fn health(&self) -> SemanticHealthComponentSnapshot {
        let nonzero = |count: usize| (count > 0).then_some(count);
        let mut fill = SemanticFillHealth {
            filling: self.filling.then_some(true),
            ..SemanticFillHealth::default()
        };
        let mut health = SemanticHealthComponentSnapshot {
            status: READY,
            reason: None,
            since_ms: None,
            next_retry_ms: None,
            paused_until_ms: None,
            stage: None,
            embedded_chunks: None,
            total_chunks: None,
            current_batch: None,
            total_batches: None,
            fill: None,
        };
        match &self.plane {
            MeaningPlane::Serving {
                complete,
                coverage,
                error,
            } => {
                health.reason.clone_from(error);
                if !complete {
                    health.status = BUILDING;
                    health.stage = Some(super::meaning::FILLING_STAGE.to_string());
                    health.embedded_chunks = Some(coverage.chunks_embedded);
                    health.total_chunks = Some(coverage.chunks);
                    fill.files = Some(coverage.files);
                    fill.files_embedded = nonzero(coverage.files_embedded);
                    fill.files_pending = nonzero(coverage.files_pending);
                    fill.files_too_large = nonzero(coverage.files_too_large);
                    fill.files_past_cap = nonzero(coverage.files_past_cap);
                }
                fill.files_failed = nonzero(coverage.files_failed);
                fill.chunks_failed = nonzero(coverage.chunks_failed);
            }
            MeaningPlane::Building { stage, files, .. } => {
                health.status = BUILDING;
                health.stage = Some(stage.clone());
                fill.files = *files;
            }
            MeaningPlane::Failed(error)
                if crate::semantic_admission::is_not_opened_status(error) =>
            {
                health.status = crate::semantic_admission::NOT_OPENED_LABEL;
            }
            MeaningPlane::Failed(error) => {
                health.status = "degraded";
                health.reason = Some(error.clone());
            }
            MeaningPlane::Disabled => health.status = "disabled",
            MeaningPlane::Ready => {}
        }
        if self.waiting {
            health.stage = Some(super::meaning::WAITING_STAGE.to_string());
        }
        if health.status == BUILDING {
            // Health's stuck-plane check: building since the fill under way
            // started, and moving whenever a fill stores. Before any fill
            // starts, health dates the building plane from when it first
            // saw it.
            health.since_ms = self.times.started_at_ms;
            fill.last_progress_at_ms = self.times.stored_at_ms;
        }
        health.fill = (fill != SemanticFillHealth::default()).then_some(fill);
        health
    }
}

/// How `checkout`'s meaning plane stands; `None` while semantic search is
/// off for the checkout.
fn meaning_report(checkout: &DataCheckout) -> Option<MeaningReport> {
    use crate::context::SemanticIndexStatus;
    use crate::data_layer::tools::search::DataSemantic;
    let semantic = DataSemantic::of_checkout(checkout)?;
    let filling = checkout.filling();
    let plane = match semantic {
        DataSemantic::Serving(meaning) => MeaningPlane::Serving {
            complete: meaning.complete(),
            coverage: MeaningCoverage::of(meaning.coverage()),
            error: meaning.failure().map(str::to_string),
        },
        DataSemantic::NotServing(SemanticIndexStatus::Building {
            stage,
            files,
            entries_done,
            entries_total,
        }) => MeaningPlane::Building {
            stage,
            files,
            entries_done,
            entries_total,
        },
        DataSemantic::NotServing(SemanticIndexStatus::Failed(error)) => MeaningPlane::Failed(error),
        DataSemantic::NotServing(SemanticIndexStatus::Disabled) => MeaningPlane::Disabled,
        DataSemantic::NotServing(SemanticIndexStatus::Ready { .. }) => MeaningPlane::Ready,
    };
    // A fill round waiting for another fill of the repository, in this
    // process or another, embeds nothing until that one publishes.
    let waiting = filling && checkout.meaning_progress().is_some_and(|step| step.waiting);
    Some(MeaningReport {
        plane,
        filling,
        waiting,
        times: checkout.meaning_fill_times(),
    })
}

/// How a bound checkout with `status` stands, given the paths of its view
/// (`None` before anything is published) and its last failure.
fn report_from(
    status: &CheckoutStatus,
    files: Option<usize>,
    failure: Option<DataFailure>,
    family_dir: Option<PathBuf>,
) -> DataStoreReport {
    let build_failure = failure.as_ref().filter(|failure| failure.stage == "build");
    let refresh_failure = failure
        .as_ref()
        .filter(|failure| failure.stage == "refresh");
    // Bound and not published: the bind started a build, which is running,
    // about to run, or failed.
    let unpublished = || match build_failure {
        Some(failure) if !status.building => PlaneReport::failed(failure),
        _ => PlaneReport::of(BUILDING),
    };
    let (text, indexed_files) = match (files, status.text) {
        // Switched off in the configuration: not built, and grep walks.
        _ if !status.planes.text => (PlaneReport::of(OFF), None),
        (Some(_), PlaneStatus::Ready) => (PlaneReport::of(READY), None),
        (Some(_), PlaneStatus::Building { done, .. }) => (PlaneReport::of(BUILDING), Some(done)),
        _ => (unpublished(), None),
    };
    // The call graph is refreshed lazily: a published graph with edits
    // waiting answers, so it is ready, with the edits counted.
    let callgraph = if !status.planes.callgraph {
        PlaneReport::of(OFF)
    } else if !status.callgraph_published {
        unpublished()
    } else if let Some(failure) = refresh_failure {
        PlaneReport::failed(failure)
    } else {
        PlaneReport::of(READY)
    };
    DataStoreReport {
        text,
        callgraph,
        files,
        indexed_files,
        pending_edits: status.callgraph_pending,
        unparsed_files: status.unparsed,
        refusal: failure.as_ref().and_then(|failure| failure.refusal.clone()),
        failure,
        family_dir,
        meaning: None,
    }
}

/// The bytes of every file under `dir`; zero when it cannot be read.
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |meta| meta.len()),
            _ => 0,
        })
        .fold(0, u64::saturating_add)
}

/// The data layer's memory for one root's checkout, measured as the
/// [`aft_store::heap`] module describes (see
/// [`super::checkout::CheckoutMemory`] for what each part holds). The text
/// plane's bytes belong in the root's `search` plane, the call graph's in
/// `callgraph`, and meaning search's in `semantic`: [`Self::fold_into`]
/// adds them there. `shared` is what the planes share, the root's
/// `data_layer` plane, with every figure itemised in its counts. The block
/// cache every checkout shares is counted once for the process
/// ([`block_cache_estimate`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DataLayerMemory {
    pub(crate) shared: MemoryEstimate,
    pub(crate) text: u64,
    pub(crate) callgraph: u64,
    pub(crate) meaning: u64,
}

impl DataLayerMemory {
    /// A bound checkout another thread is measuring right now.
    pub(crate) fn busy_checkout() -> Self {
        Self {
            shared: MemoryEstimate::busy()
                .gap("checkout_memory_busy")
                .count("bound_checkouts", 1),
            ..Self::unbound()
        }
    }

    /// Nothing held: no checkout is bound.
    fn unbound() -> Self {
        Self {
            shared: MemoryEstimate::estimated(0).count("bound_checkouts", 0),
            text: 0,
            callgraph: 0,
            meaning: 0,
        }
    }

    /// Adds the text, call-graph and meaning bytes to the root's `search`,
    /// `callgraph` and `semantic` planes, each with its share named
    /// `data_layer_bytes`, and returns the root's `data_layer` plane: what
    /// the planes share.
    pub(crate) fn fold_into(
        self,
        search: &mut MemoryEstimate,
        callgraph: &mut MemoryEstimate,
        semantic: &mut MemoryEstimate,
    ) -> MemoryEstimate {
        for (plane, bytes) in [
            (search, self.text),
            (callgraph, self.callgraph),
            (semantic, self.meaning),
        ] {
            plane.counts.insert("data_layer_bytes".into(), bytes);
            if bytes == 0 {
                continue;
            }
            plane.estimated_bytes = Some(match plane.estimated_bytes {
                Some(own) => own.saturating_add(bytes),
                // Today's store for the plane could not be read; the data
                // layer's share still counts, and the plane says it is
                // partial.
                None => {
                    plane.bytes_status = "partial";
                    bytes
                }
            });
        }
        self.shared
    }
}

/// The data layer's memory for `ctx`'s root (see [`DataLayerMemory`]);
/// `None` when the data layer is off. Never binds the checkout and never
/// waits for a lock.
pub(crate) fn memory_estimate(ctx: &AppContext) -> Option<DataLayerMemory> {
    Some(match ctx.probe_data_checkout()? {
        // Nothing is held for a checkout that is not bound.
        DataCheckoutProbe::NotBound
        | DataCheckoutProbe::BindFailed(_)
        | DataCheckoutProbe::Withheld(_) => DataLayerMemory::unbound(),
        DataCheckoutProbe::Binding => DataLayerMemory {
            shared: MemoryEstimate::busy().gap("checkout_binding"),
            ..DataLayerMemory::unbound()
        },
        DataCheckoutProbe::Bound(checkout) => checkout.memory_figure(|| {
            let mut seen = aft_store::heap::Seen::new();
            // aft_inspect's dead-code scan may hold the same snapshot the
            // checkout's dead-code memo holds; the root's
            // `callgraph_projection` plane counts it then.
            if let Some(snapshot) = ctx.inspect_manager().try_callgraph_projection_snapshot() {
                seen.counted_elsewhere(Arc::as_ptr(&snapshot));
            }
            let memory = checkout.checkout().memory(&mut seen);
            let meaning = checkout.meaning_memory(&mut seen);
            checkout_estimate(memory, meaning, seen.contended())
        }),
    })
}

/// A bound checkout's memory as the census reports it, from what its walk
/// measured: `meaning` is meaning search's bytes (`None` when its plane's
/// slot was held), and `contended` how many parts the whole walk left out
/// because another thread held them. A part left out is named, so a figure
/// that misses it reads as partial, never as complete.
pub(crate) fn checkout_estimate(
    memory: CheckoutMemory,
    meaning: Option<usize>,
    contended: usize,
) -> DataLayerMemory {
    let meaning = meaning.map(|bytes| u64::try_from(bytes).unwrap_or(u64::MAX));
    let Some(live) = memory.live else {
        return DataLayerMemory {
            shared: MemoryEstimate::busy()
                .gap("checkout_state_busy")
                .count("bound_checkouts", 1),
            ..DataLayerMemory::unbound()
        };
    };
    let (engine_state, engine_bytes) = match memory.engine {
        EngineMemory::Running {
            state_bytes, bytes, ..
        } => (state_bytes, bytes),
        EngineMemory::NotStarted | EngineMemory::Busy => (0, 0),
    };
    let shared_bytes = live.shared_bytes.saturating_add(engine_state);
    let callgraph = live.callgraph_bytes.saturating_add(engine_bytes);
    let mut shared = MemoryEstimate::estimated(shared_bytes)
        .count("bound_checkouts", 1)
        .count("state_paths", live.state_paths)
        .count("text_overlay_paths", live.text_overlay_paths)
        .count_u64("text_overlay_bytes", live.text_overlay_bytes)
        .count("changed_paths", live.changes)
        .count("pending_callgraph_paths", live.pending_callgraph_paths)
        .count("refreshed_callgraph_files", live.refreshed_files)
        // Already in the root's `search`, `callgraph` and `semantic` planes.
        .count_u64("search_bytes", live.text_bytes)
        .count_u64("callgraph_bytes", callgraph)
        .count_u64("semantic_bytes", meaning.unwrap_or(0));
    if let Some(files) = live.dead_code_files {
        shared = shared.count("dead_code_snapshot_files", files);
    }
    shared = match memory.engine {
        EngineMemory::NotStarted => shared.count("callgraph_engine_started", 0),
        EngineMemory::Busy => shared
            .count("callgraph_engine_started", 1)
            .gap("callgraph_engine_busy"),
        EngineMemory::Running {
            parses,
            resolutions,
            bytes,
            ..
        } => shared
            .count("callgraph_engine_started", 1)
            .count("callgraph_engine_parses", parses)
            .count("callgraph_engine_resolutions", resolutions)
            .count_u64("callgraph_engine_bytes", bytes),
    };
    if meaning.is_none() {
        shared = shared.gap("meaning_plane_busy");
    }
    if contended > 0 {
        shared = shared.count("locked_parts", contended).gap("locked_parts");
    }
    if !shared.not_estimated.is_empty() {
        shared.bytes_status = "partial";
    }
    DataLayerMemory {
        shared,
        text: live.text_bytes,
        callgraph,
        meaning: meaning.unwrap_or(0),
    }
}

/// The process's shared block cache as a census estimate, counted once for
/// every checkout; `None` while no checkout of this process has bound. Its bytes are measured as the
/// [`aft_store::heap`] module describes: every cached block in its shared
/// allocation and the cache's two lookup tables. `resident_bytes` is the
/// block data alone, which the cache keeps within `budget_bytes`. While
/// another thread holds the cache's lock the tables are left out and the
/// figure is partial.
pub(crate) fn block_cache_estimate() -> Option<MemoryEstimate> {
    let (stats, measured) = super::runtime::block_cache_memory()?;
    let resident = u64::try_from(stats.resident_bytes).unwrap_or(u64::MAX);
    let estimate = match measured {
        Some(bytes) => MemoryEstimate::estimated(u64::try_from(bytes).unwrap_or(u64::MAX)),
        None => MemoryEstimate::partial(resident).gap("block_cache_tables_busy"),
    };
    Some(
        estimate
            .count_u64("resident_bytes", resident)
            .count("budget_bytes", stats.budget_bytes)
            .count("blocks", stats.blocks)
            .count_u64("hits", stats.hits)
            .count_u64("misses", stats.misses)
            .count_u64("evictions", stats.evictions),
    )
}

/// Rewrites a status payload for the data layer: `search_index` and
/// `callgraph_store` describe its planes, `data_store` its pending edits
/// and failures, and `disk` its family store. The `views` object and the
/// old call-graph store's refresh queue (`callgraph_refresh`) are dropped:
/// neither describes what these planes answer from.
pub(crate) fn apply_to_status(payload: &mut Value, report: &DataStoreReport) {
    payload["search_index"] = report.search_index_json();
    if let Some(meaning) = &report.meaning {
        let meaning = meaning.status_json();
        // An embedding backend that is paused or cannot be reached is
        // reported as today's status reports it, whichever store embeds:
        // its object keeps its words, and gains the plane's counts.
        let backend_down = matches!(
            payload["semantic_index"]["status"].as_str(),
            Some("paused" | "backend_unavailable")
        );
        let mut semantic = if backend_down {
            let mut semantic = payload["semantic_index"].clone();
            if let (Some(semantic), Some(meaning)) = (semantic.as_object_mut(), meaning.as_object())
            {
                for (key, value) in meaning {
                    if !matches!(key.as_str(), "status" | "state" | "reason" | "error") {
                        semantic.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
            }
            semantic
        } else {
            meaning
        };
        // The model and backend labels still come from the configuration.
        for label in ["backend", "model"] {
            if let Some(value) = payload["semantic_index"].get(label).cloned() {
                semantic[label] = value;
            }
        }
        payload["semantic_index"] = semantic;
    }
    payload["callgraph_store"] = report.callgraph_store_json();
    payload["data_store"] = serde_json::to_value(report.health()).unwrap_or(Value::Null);
    if let (Some(disk), Some(dir)) = (
        payload.get_mut("disk").and_then(Value::as_object_mut),
        &report.family_dir,
    ) {
        disk.insert("data_store_dir".into(), json!(dir.display().to_string()));
        disk.insert("data_store_bytes".into(), json!(dir_bytes(dir)));
    }
    if let Some(object) = payload.as_object_mut() {
        object.remove("views");
        object.remove("callgraph_refresh");
    }
}

/// zoom's `callgraph` field when the data layer is on: `Some(None)` while
/// the call graph answers (ready, or published with edits waiting, which
/// is normal), `Some(Some(field))` otherwise, in today's shape. `None` when
/// the data layer is off, so zoom asks today's store. Like every status
/// read, it never binds the checkout.
pub(crate) fn zoom_callgraph_field(ctx: &AppContext) -> Option<Option<Value>> {
    let report = report(ctx)?;
    let plane = &report.callgraph;
    // `callgraph_off` tells the agent the call graph is switched off in
    // config. A root that is merely not bound yet is off in status, but its
    // call graph is unavailable, not switched off.
    let code = match plane.status {
        READY => return Some(None),
        OFF if plane.reason != Some(reason::NOT_BOUND) => "callgraph_off",
        BUILDING => "callgraph_building",
        _ => "callgraph_unavailable",
    };
    Some(Some(json!({
        "status": "unavailable",
        "code": code,
        "index": { "status": plane.status, "reason": plane.reason },
    })))
}

/// A plane as `aft warmup` waits for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarmState {
    /// Still building, with a word on what it waits for.
    Pending(String),
    Ready,
    /// The build failed; waiting longer would not change it.
    Failed(String),
}

/// The text and call-graph planes of `ctx`'s root, for `aft warmup`.
/// Binds the checkout on the first call, which starts its build; a call
/// graph that answers with edits still waiting counts as ready.
pub fn warm(ctx: &AppContext) -> (WarmState, WarmState) {
    let report = match ctx.data_checkout() {
        Some(checkout) => report_of(&checkout),
        None => match report(ctx) {
            Some(report) => report,
            None => {
                let off = WarmState::Failed("the data layer is off".to_string());
                return (off.clone(), off);
            }
        },
    };
    (
        warm_state(&report.text, report.files, report.indexed_files),
        warm_state(&report.callgraph, None, None),
    )
}

fn warm_state(plane: &PlaneReport, files: Option<usize>, indexed: Option<usize>) -> WarmState {
    match plane.status {
        READY => WarmState::Ready,
        BUILDING => WarmState::Pending(match (indexed, files) {
            (Some(indexed), Some(files)) => format!("building, files={indexed}/{files}"),
            _ => "building".to_string(),
        }),
        _ => {
            let reason = plane.reason.unwrap_or(plane.status);
            WarmState::Failed(match &plane.error {
                // A refusal's message already starts with its code.
                Some(error) if error.starts_with(reason) => error.clone(),
                Some(error) => format!("{reason}: {error}"),
                None => reason.to_string(),
            })
        }
    }
}

/// What `aft index` left published for one checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexOutcome {
    /// Paths of the checkout.
    pub files: usize,
    /// Files the call graph covers.
    pub parsed_files: usize,
    /// Files in a parsed language the call graph does not cover, because
    /// extracting them failed.
    pub unparsed_files: usize,
    /// Whether this run published; `false` when the store already held
    /// exactly what is on disk.
    pub published: bool,
}

/// Builds and publishes the data layer's `planes` for the checkout at
/// `root`, with its family store under `storage`, in the foreground;
/// nothing is published when the store already holds exactly what is on
/// disk.
///
/// # Errors
///
/// When the store cannot be opened, the build fails, or other AFT
/// processes kept publishing the checkout first.
pub fn index_root(
    root: &Path,
    storage: &Path,
    planes: super::checkout::Planes,
) -> Result<IndexOutcome, String> {
    let checkout = super::runtime::bind_foreground(root, storage, planes)
        .map_err(|error| error.to_string())?;
    let (view, published) = publish_current(&checkout, planes)?;
    let state = view.state();
    Ok(IndexOutcome {
        files: state.paths().len(),
        parsed_files: (0..state.paths().len())
            .filter(|index| state.fragment(*index).is_some())
            .count(),
        unparsed_files: view.status().unparsed,
        published,
    })
}

/// Builds and publishes `planes` of `checkout` until its view holds exactly
/// the files on disk; returns that view and whether this call published.
///
/// # Errors
///
/// When a build fails, or other AFT processes kept publishing the checkout
/// first.
fn publish_current(
    checkout: &super::checkout::Checkout,
    planes: super::checkout::Planes,
) -> Result<(CheckoutView, bool), String> {
    /// Builds that may stand down for another process's publication before
    /// this one gives up.
    const ATTEMPTS: usize = 3;
    // Binding compares the files on disk with the publication and records
    // any difference, so a current store has a view with nothing changed.
    let current = |checkout: &super::checkout::Checkout| {
        checkout.view().is_some_and(|view| {
            let status = view.status();
            let ready = if planes.callgraph {
                status.callgraph
            } else {
                status.text
            };
            view.changed_paths().is_empty() && ready == PlaneStatus::Ready
        })
    };
    let mut published = false;
    for _ in 0..ATTEMPTS {
        if current(checkout) {
            break;
        }
        let report = checkout.build().map_err(|error| error.to_string())?;
        published |= !report.superseded;
    }
    let view = checkout
        .view()
        .filter(|_| current(checkout))
        .ok_or_else(|| {
            "other AFT processes kept publishing this checkout while it was built".to_string()
        })?;
    Ok((view, published))
}

/// What `aft index` left filled in a checkout's meaning plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeaningOutcome {
    /// Files meaning search applies to, past `semantic.max_files` included.
    pub files: usize,
    /// Files with vectors.
    pub files_embedded: usize,
    /// Files too large to embed, which have none by design.
    pub files_too_large: usize,
    /// Files that could not be cut into chunks, with none.
    pub files_failed: usize,
    /// Files past `semantic.max_files`, left unfilled.
    pub files_past_cap: usize,
    /// Chunks with vectors.
    pub chunks: usize,
}

/// Publishes the text plane of the checkout at `root`, with its family store
/// under `storage`, as [`index_root`] does, then fills its meaning plane with
/// `config`'s embedding model in the foreground until every file meaning
/// search applies to has vectors or a recorded reason it has none. Vectors
/// another checkout of the repository embedded are reused, not embedded
/// again; a fill of the repository running in another process is waited
/// for, as the plane's fill lock makes every fill wait.
///
/// # Errors
///
/// When the store cannot be opened, the build fails, or a fill round fails
/// (the embedding model cannot load, or the backend refuses or cannot be
/// reached), with the message the status tool's `semantic_index.error`
/// would carry for that failure.
pub fn fill_root(
    root: &Path,
    storage: &Path,
    config: &crate::config::SemanticBackendConfig,
) -> Result<MeaningOutcome, String> {
    use super::meaning::{Meaning, MeaningState, Round};
    let planes = super::checkout::Planes {
        text: true,
        callgraph: false,
    };
    let checkout = Arc::new(
        super::runtime::bind_foreground(root, storage, planes)
            .map_err(|error| error.to_string())?,
    );
    publish_current(&checkout, planes)?;
    let meaning = Meaning::new(storage);
    meaning.configure(Some(config));
    loop {
        match meaning.round(&checkout) {
            Round::More => {}
            Round::Done => break,
            Round::Idle => return Err("the checkout has nothing published to fill".to_string()),
            Round::Failed(failure) => return Err(failure.message().to_string()),
        }
    }
    match meaning.state(checkout.view().as_ref()) {
        MeaningState::Serving { snapshot, .. } => {
            let coverage = snapshot.coverage();
            Ok(MeaningOutcome {
                files: coverage.files,
                files_embedded: coverage.files_embedded,
                files_too_large: coverage.files_too_large,
                files_failed: coverage.files_failed.len(),
                files_past_cap: coverage.past_cap,
                chunks: coverage.chunks_embedded,
            })
        }
        MeaningState::Blocked(failure) => Err(failure.message().to_string()),
        MeaningState::Off | MeaningState::Unpublished | MeaningState::Starting => {
            Err("the meaning plane could not be read after its fill".to_string())
        }
    }
}

/// The meaning plane of `ctx`'s root, for `aft warmup`, when it serves the
/// root's semantic search (the data layer binds the root, and semantic
/// search is on and admitted for it); `None` otherwise, and today's semantic
/// index status applies. Asking binds
/// the checkout and starts its fill. Ready only once every file meaning
/// search applies to is filled: a partial plane is still pending.
pub fn warm_meaning(ctx: &AppContext) -> Option<WarmState> {
    use crate::context::SemanticIndexStatus;
    use crate::data_layer::tools::search::DataSemantic;
    if !ctx.data_layer_serves_meaning() {
        return None;
    }
    let Some(data) = DataSemantic::current(ctx) else {
        // The checkout is not bound yet, or its store cannot be opened; the
        // text plane's warm state names a failure to open it.
        return Some(WarmState::Pending("binding".to_string()));
    };
    Some(match data {
        DataSemantic::Serving(meaning) if meaning.complete() => WarmState::Ready,
        DataSemantic::Serving(meaning) => {
            let coverage = meaning.coverage();
            let accounted =
                coverage.files_embedded + coverage.files_too_large + coverage.files_failed.len();
            let total = coverage.files.saturating_sub(coverage.past_cap);
            let mut detail = format!("filling, files={accounted}/{total}");
            if let Some(failure) = meaning.failure() {
                detail.push_str(&format!(", last failure: {failure}"));
            }
            WarmState::Pending(detail)
        }
        DataSemantic::NotServing(SemanticIndexStatus::Failed(error)) => WarmState::Failed(error),
        DataSemantic::NotServing(SemanticIndexStatus::Building {
            stage,
            files,
            entries_done,
            entries_total,
        }) => {
            let mut detail = stage;
            if let Some(files) = files {
                detail.push_str(&format!(", files={files}"));
            }
            if let (Some(done), Some(total)) = (entries_done, entries_total) {
                detail.push_str(&format!(", entries={done}/{total}"));
            }
            WarmState::Pending(detail)
        }
        DataSemantic::NotServing(status) => WarmState::Pending(format!("{status:?}")),
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::{json, Value};

    use super::*;
    use crate::data_layer::checkout::{CheckoutStatus, DataError, LiveMemory};
    use crate::protocol::RawRequest;

    fn write(root: &Path, path: &str, text: &str) -> PathBuf {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A configured context over a small Rust crate, with its own storage.
    fn context(root: &Path, storage: &Path) -> AppContext {
        context_with(root, storage, |_| {})
    }

    /// As [`context`], with `edit` applied to its configuration.
    fn context_with(
        root: &Path,
        storage: &Path,
        edit: impl FnOnce(&mut crate::config::Config),
    ) -> AppContext {
        write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(
            root,
            "src/lib.rs",
            "mod util;\npub fn caller() { util::callee(); }\n",
        );
        write(root, "src/util.rs", "pub fn callee() {}\n");
        let mut config = crate::config::Config {
            project_root: Some(root.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            ..crate::config::Config::default()
        };
        edit(&mut config);
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.set_canonical_cache_root(root.to_path_buf());
        ctx
    }

    #[test]
    fn an_idle_release_drops_the_checkout_and_its_next_use_serves_the_publication() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = context(&root, storage.path());
        assert!(ctx.data_checkout().is_some());
        settled(&ctx);

        // The reaper's release after `idle.checkout_hours` without a call.
        assert!(ctx.try_release_idle_checkout_views());
        assert!(matches!(
            ctx.probe_data_checkout(),
            Some(DataCheckoutProbe::NotBound)
        ));
        // The state stayed on disk: the next use binds it and answers at
        // once, without a build.
        let checkout = ctx.data_checkout().unwrap();
        assert!(checkout.view().is_some());
        assert!(!checkout.checkout().needs_build());
    }

    #[test]
    fn roots_configure_leaves_unindexed_and_switched_off_indexes_bind_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = context(&root, storage.path());
        let plane = |ctx: &AppContext, key: &str| {
            let payload = ctx.build_status_snapshot();
            (
                payload[key]["status"].as_str().unwrap().to_string(),
                payload[key]["reason"].as_str().map(str::to_string),
            )
        };
        let unavailable = |reason: &str| (UNAVAILABLE.to_string(), Some(reason.to_string()));
        let off = |reason: Option<&str>| (OFF.to_string(), reason.map(str::to_string));

        // The home folder and a filesystem root, as configure marks them.
        ctx.set_degraded_reasons(vec!["home_root".to_string()]);
        assert!(ctx.data_checkout().is_none());
        assert!(matches!(
            ctx.probe_data_checkout(),
            Some(DataCheckoutProbe::Withheld(Withheld::Home))
        ));
        assert_eq!(plane(&ctx, "search_index"), unavailable(reason::HOME_ROOT));
        assert_eq!(
            plane(&ctx, "callgraph_store"),
            unavailable(reason::HOME_ROOT)
        );
        ctx.set_degraded_reasons(vec!["root_too_broad".to_string()]);
        assert!(ctx.data_checkout().is_none());
        assert_eq!(
            plane(&ctx, "callgraph_store"),
            off(Some(reason::ROOT_TOO_BROAD))
        );
        assert!(
            !storage.path().join("data").exists(),
            "no store was opened for either"
        );

        // Both indexes switched off: nothing to build.
        let none = context_with(&root, storage.path(), |config| {
            config.indexes.trigram = false;
            config.indexes.callgraph = false;
        });
        assert!(none.data_checkout().is_none());
        assert_eq!(plane(&none, "search_index"), off(None));
        assert!(!storage.path().join("data").exists());

        // The text index alone switched off: the call graph is built, and
        // grep gets no view, so it walks the folder.
        let graph_only = context_with(&root, storage.path(), |config| {
            config.indexes.trigram = false;
        });
        assert!(graph_only.data_checkout().is_some());
        let settled_graph = {
            let started = Instant::now();
            loop {
                let report = report(&graph_only).unwrap();
                if report.callgraph.status == READY || started.elapsed() > Duration::from_secs(60) {
                    break report;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        assert_eq!(
            (settled_graph.text.status, settled_graph.callgraph.status),
            (OFF, READY)
        );
        assert!(graph_only.data_view().is_none());
    }

    /// Waits up to 60 s for both planes of `ctx`'s bound checkout to be
    /// ready with no edit pending.
    fn settled(ctx: &AppContext) -> DataStoreReport {
        let started = Instant::now();
        loop {
            let report = report(ctx).expect("data layer on");
            if report.text.status == READY
                && report.callgraph.status == READY
                && report.pending_edits == 0
            {
                return report;
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "never settled: {report:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn zoom(ctx: &AppContext, file: &Path) -> Value {
        let request = RawRequest {
            id: "zoom".into(),
            command: "zoom".into(),
            lsp_hints: None,
            session_id: None,
            params: json!({"file": file, "symbol": "caller", "callgraph": true}),
        };
        let response = crate::commands::zoom::handle_zoom(&request, ctx);
        assert!(response.success, "{:?}", response.data);
        response.data
    }

    fn failed_with(failure: Option<(&'static str, bool)>) -> Option<DataFailure> {
        failure.map(|(stage, busy)| DataFailure {
            stage,
            error: format!("{stage} broke"),
            refusal: None,
            busy,
            at_ms: 7,
        })
    }

    fn checkout_status(
        text: PlaneStatus,
        callgraph_published: bool,
        callgraph_pending: usize,
        building: bool,
    ) -> CheckoutStatus {
        CheckoutStatus {
            text,
            callgraph: if callgraph_published && callgraph_pending == 0 {
                PlaneStatus::Ready
            } else {
                PlaneStatus::Building {
                    done: 0,
                    total: None,
                }
            },
            callgraph_published,
            callgraph_pending,
            building,
            unparsed: 1,
            failed: false,
            planes: crate::data_layer::checkout::Planes::ALL,
        }
    }

    #[test]
    fn each_checkout_state_maps_to_one_plane_state() {
        let unbuilt = PlaneStatus::Off;
        // Bound, nothing published, the bind's build running or about to.
        let report = report_from(&checkout_status(unbuilt, false, 0, true), None, None, None);
        assert_eq!(
            (report.text.status, report.callgraph.status),
            (BUILDING, BUILDING)
        );
        let report = report_from(&checkout_status(unbuilt, false, 0, false), None, None, None);
        assert_eq!(
            (report.text.status, report.callgraph.status),
            (BUILDING, BUILDING)
        );

        // The build failed: both planes unavailable with its reason.
        let failed = report_from(
            &checkout_status(unbuilt, false, 0, false),
            None,
            failed_with(Some(("build", false))),
            None,
        );
        assert_eq!(failed.text.status, UNAVAILABLE);
        assert_eq!(failed.callgraph.reason, Some(reason::BUILD_FAILED));
        assert_eq!(failed.text.error.as_deref(), Some("build broke"));
        let busy = report_from(
            &checkout_status(unbuilt, false, 0, false),
            None,
            failed_with(Some(("build", true))),
            None,
        );
        assert_eq!(busy.text.reason, Some(reason::STORE_BUSY));
        // A build running again after a failure is building.
        let retried = report_from(
            &checkout_status(unbuilt, false, 0, true),
            None,
            failed_with(Some(("build", false))),
            None,
        );
        assert_eq!(retried.text.status, BUILDING);

        // Text published first, call graph still building; text with some
        // files not indexed yet reports how far it got.
        let text_first = report_from(
            &checkout_status(PlaneStatus::Ready, false, 0, true),
            Some(5),
            None,
            None,
        );
        assert_eq!(
            (text_first.text.status, text_first.callgraph.status),
            (READY, BUILDING)
        );
        let partial = report_from(
            &checkout_status(
                PlaneStatus::Building {
                    done: 3,
                    total: Some(5),
                },
                true,
                0,
                false,
            ),
            Some(5),
            None,
            None,
        );
        assert_eq!(partial.text.status, BUILDING);
        assert_eq!(
            partial.search_index_json(),
            json!({"status": "building", "files": 5, "indexed_files": 3})
        );

        // A published call graph with edits waiting is ready, the edits
        // counted: the lazy state, never building.
        let pending = report_from(
            &checkout_status(PlaneStatus::Ready, true, 3, false),
            Some(5),
            None,
            None,
        );
        assert_eq!(pending.callgraph.status, READY);
        assert_eq!(
            pending.callgraph_store_json(),
            json!({"status": "ready", "pending_edits": 3, "unparsed_files": 1})
        );
        assert_eq!(pending.health().pending_edits, 3);

        // A refresh that failed makes it unavailable; a rebuild that failed
        // while the publication still serves leaves it ready but reports the
        // failure.
        let refresh = report_from(
            &checkout_status(PlaneStatus::Ready, true, 3, false),
            Some(5),
            failed_with(Some(("refresh", false))),
            None,
        );
        assert_eq!(refresh.callgraph.status, UNAVAILABLE);
        assert_eq!(refresh.callgraph.reason, Some(reason::REFRESH_FAILED));
        assert_eq!(refresh.text.status, READY);
        let rebuild = report_from(
            &checkout_status(PlaneStatus::Ready, true, 0, false),
            Some(5),
            failed_with(Some(("build", false))),
            None,
        );
        assert_eq!(
            (rebuild.text.status, rebuild.callgraph.status),
            (READY, READY)
        );
        let health = serde_json::to_value(rebuild.health()).unwrap();
        assert_eq!(
            health,
            json!({"pending_edits": 0, "unparsed_files": 1,
                "failure": {"stage": "build", "error": "build broke", "busy": false, "at_ms": 7}})
        );
    }

    #[test]
    fn status_health_and_zoom_follow_the_checkout_through_each_state() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = context(&root, storage.path());
        let lib = root.join("src/lib.rs");

        // Not bound: off, with the reason, and settled; reading never binds.
        let payload = ctx.build_status_snapshot();
        assert_eq!(
            payload["search_index"],
            json!({"status": "off", "reason": "not_bound"})
        );
        assert_eq!(payload["callgraph_store"]["status"], "off");
        assert_eq!(payload["data_store"]["reason"], "not_bound");
        assert!(payload.get("callgraph_refresh").is_none());
        assert!(payload["disk"].get("data_store_bytes").is_none());
        let health = ctx.try_health_snapshot(&root);
        assert_eq!(health.search_index.unwrap().status, "off");
        assert_eq!(health.callgraph_store.unwrap().status, "off");
        assert_eq!(health.data_store.unwrap().reason, Some(reason::NOT_BOUND));
        assert_eq!(
            zoom(&ctx, &lib)["callgraph"],
            json!({"status": "unavailable", "code": "callgraph_unavailable",
                "index": {"status": "off", "reason": "not_bound"}})
        );
        assert!(
            matches!(ctx.probe_data_checkout(), Some(DataCheckoutProbe::NotBound)),
            "status, health and zoom bound nothing"
        );

        // A bind in progress: building everywhere, never ready.
        {
            let _binding = ctx.hold_data_checkout_slot_for_test();
            let payload = ctx.build_status_snapshot();
            assert_eq!(payload["search_index"]["status"], "building");
            assert_eq!(payload["callgraph_store"]["status"], "building");
            let health = ctx.try_health_snapshot(&root);
            assert_eq!(health.search_index.unwrap().status, "building");
            assert_eq!(health.callgraph_store.unwrap().status, "building");
            assert_eq!(
                zoom_callgraph_field(&ctx),
                Some(Some(
                    json!({"status": "unavailable", "code": "callgraph_building",
                    "index": {"status": "building", "reason": null}})
                ))
            );
        }

        // Bound and built: ready, with the file count and the store's size.
        assert!(ctx.data_checkout().is_some());
        let ready = settled(&ctx);
        assert_eq!(ready.files, Some(3));
        let payload = ctx.build_status_snapshot();
        assert_eq!(
            payload["search_index"],
            json!({"status": "ready", "files": 3})
        );
        assert_eq!(
            payload["callgraph_store"],
            json!({"status": "ready", "pending_edits": 0, "unparsed_files": 0})
        );
        assert!(payload["disk"]["data_store_bytes"].as_u64().unwrap() > 0);
        assert!(payload["disk"]["data_store_dir"]
            .as_str()
            .unwrap()
            .starts_with(&storage.path().display().to_string()));
        assert!(payload.get("views").is_none());
        let health = ctx.try_health_snapshot(&root);
        assert_eq!(health.search_index.unwrap().status, "ready");
        assert_eq!(health.callgraph_store.unwrap().status, "ready");
        assert_eq!(
            serde_json::to_value(health.data_store.unwrap()).unwrap(),
            json!({"pending_edits": 0, "unparsed_files": 0})
        );
        assert!(zoom(&ctx, &lib).get("callgraph").is_none());

        // An edit waits for the lazy refresh: still ready, the edit counted,
        // zoom quiet.
        let extra = write(
            &root,
            "src/extra.rs",
            "pub fn other() { crate::util::callee(); }\n",
        );
        ctx.data_note_paths(&[extra]);
        let payload = ctx.build_status_snapshot();
        assert_eq!(payload["callgraph_store"]["status"], "ready");
        assert!(
            payload["callgraph_store"]["pending_edits"]
                .as_u64()
                .unwrap()
                >= 1,
            "{payload:?}"
        );
        assert_eq!(
            ctx.try_health_snapshot(&root)
                .callgraph_store
                .unwrap()
                .status,
            "ready"
        );
        assert!(zoom(&ctx, &lib).get("callgraph").is_none());

        // A failed refresh: unavailable with its reason and error, in health
        // with when it failed, and in zoom; the next success clears it.
        let checkout = ctx.data_checkout().expect("bound");
        checkout.note_outcome_for_test::<()>(
            "refresh",
            &Err(DataError::Resolve("resolver gave up".into())),
        );
        let payload = ctx.build_status_snapshot();
        assert_eq!(payload["callgraph_store"]["status"], "unavailable");
        assert_eq!(payload["callgraph_store"]["reason"], "refresh_failed");
        assert_eq!(
            payload["callgraph_store"]["error"],
            "AFT could not build this checkout's call graph: resolver gave up"
        );
        assert_eq!(payload["search_index"]["status"], "ready");
        let health = ctx.try_health_snapshot(&root);
        assert_eq!(health.callgraph_store.unwrap().status, "unavailable");
        let failure = health.data_store.unwrap().failure.unwrap();
        assert_eq!((failure.stage, failure.busy), ("refresh", false));
        assert!(failure.at_ms > 0);
        assert_eq!(
            zoom_callgraph_field(&ctx).unwrap().unwrap()["code"],
            "callgraph_unavailable"
        );
        checkout.note_outcome_for_test("refresh", &Ok(()));
        assert_eq!(report(&ctx).unwrap().callgraph.status, READY);
    }

    #[test]
    fn a_store_that_cannot_open_reports_why_on_status_and_health() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        // A file where the storage folder should be: the family store
        // cannot be created under it.
        let blocked = storage.path().join("blocked");
        std::fs::write(&blocked, "").unwrap();
        let ctx = context(&root, &blocked);
        assert!(ctx.data_checkout().is_none());

        let payload = ctx.build_status_snapshot();
        assert_eq!(payload["search_index"]["status"], "unavailable");
        assert_eq!(payload["search_index"]["reason"], "bind_failed");
        assert_eq!(payload["callgraph_store"]["reason"], "bind_failed");
        assert!(payload["data_store"]["error"]
            .as_str()
            .unwrap()
            .contains("index store"));
        let health = ctx.try_health_snapshot(&root);
        assert_eq!(health.search_index.unwrap().status, "unavailable");
        assert_eq!(health.data_store.unwrap().reason, Some(reason::BIND_FAILED));
        assert_eq!(
            ctx.memory_root_snapshot().data_layer.unwrap().counts["bound_checkouts"],
            0
        );
    }

    #[test]
    fn the_memory_census_counts_the_checkout_and_the_shared_block_cache() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = context(&root, storage.path());
        let data_layer_bytes =
            |plane: &MemoryEstimate| plane.counts.get("data_layer_bytes").copied();
        // The root's census row and the sum of its planes.
        let census_row = |memory: &crate::memory::RootMemorySnapshot| {
            let snapshot = crate::memory::MemorySnapshot::new(
                "ready",
                [(root.display().to_string(), memory.clone())].into(),
            );
            let census = crate::commands::memory_census::render_memory_census(&snapshot, None);
            let row = census["roots"][root.display().to_string()].clone();
            let planes = row["planes"]
                .as_object()
                .unwrap()
                .values()
                .map(|bytes| bytes.as_u64().unwrap())
                .sum::<u64>();
            (row, planes)
        };

        // Not bound: nothing held, said so.
        let unbound = ctx.memory_root_snapshot();
        let shared = unbound.data_layer.clone().unwrap();
        assert_eq!(shared.estimated_bytes, Some(0));
        assert_eq!(shared.counts["bound_checkouts"], 0);
        for plane in [&unbound.trigram, &unbound.callgraph, &unbound.semantic] {
            assert_eq!(data_layer_bytes(plane), Some(0));
        }
        {
            let _binding = ctx.hold_data_checkout_slot_for_test();
            let binding = ctx.memory_root_snapshot();
            assert_eq!(binding.data_layer.as_ref().unwrap().status, "busy");
            assert_eq!(binding.status, "busy", "a busy part makes the root busy");
        }

        ctx.data_checkout().unwrap();
        settled(&ctx);
        // A query reads through the block cache.
        let view = ctx.data_view().unwrap();
        view.path_text(b"src/lib.rs").unwrap();
        drop(view);
        let root_memory = ctx.memory_root_snapshot();
        let shared = root_memory.data_layer.clone().unwrap();
        assert_eq!(shared.status, "ready");
        // Every part is measured: nothing is left as a gap.
        assert_eq!(shared.bytes_status, "estimated");
        assert!(shared.not_estimated.is_empty(), "{shared:?}");
        assert!(shared.estimated_bytes.unwrap() > 0);
        assert_eq!(shared.counts["bound_checkouts"], 1);
        assert_eq!(shared.counts["state_paths"], 3);
        // Each plane's share is in the plane, and itemised.
        let search = shared.counts["search_bytes"];
        let callgraph = shared.counts["callgraph_bytes"];
        assert!(search > 0 && callgraph > 0, "{shared:?}");
        assert_eq!(data_layer_bytes(&root_memory.trigram), Some(search));
        assert_eq!(data_layer_bytes(&root_memory.callgraph), Some(callgraph));
        assert_eq!(
            data_layer_bytes(&root_memory.semantic),
            Some(shared.counts["semantic_bytes"])
        );
        assert!(root_memory.trigram.estimated_bytes.unwrap() >= search);
        assert!(root_memory.callgraph.estimated_bytes.unwrap() >= callgraph);

        let snapshot = crate::memory::MemorySnapshot::new(
            "ready",
            [(root.display().to_string(), root_memory.clone())].into(),
        );
        let cache = snapshot.data_layer_block_cache.clone().unwrap();
        assert_eq!(cache.bytes_status, "estimated");
        assert!(cache.counts["budget_bytes"] > 0);
        assert!(cache.counts["blocks"] > 0);
        assert!(
            cache.estimated_bytes.unwrap() > cache.counts["resident_bytes"],
            "the cache's figure is its blocks and its tables: {cache:?}"
        );
        assert_eq!(
            snapshot.process.data_layer_block_cache_bytes,
            cache.estimated_bytes
        );
        let (row, planes_sum) = census_row(&root_memory);
        assert_eq!(
            row["planes"]["data_layer"],
            json!(shared.estimated_bytes.unwrap())
        );
        assert_eq!(
            row["planes"]["search"],
            json!(root_memory.trigram.estimated_bytes.unwrap())
        );
        assert_eq!(row["data_layer"]["search_bytes"], json!(search));

        // Nothing is counted twice: the root's total is the sum of its
        // planes, and the process's total is the sum of its roots and of
        // what it counts once for every root.
        assert_eq!(row["attributed_bytes"], json!(planes_sum));
        assert_eq!(root_memory.attributed_bytes, planes_sum);
        let process = &snapshot.process;
        assert_eq!(
            process.total_attributed_bytes,
            planes_sum
                + snapshot.shared_semantic_bases.estimated_bytes.unwrap_or(0)
                + process.data_layer_block_cache_bytes.unwrap()
                + process.embedding_model_bytes
                + process.sqlite.memory_used_bytes
        );

        // An edit adds its overlay, and its refresh starts the call graph's
        // engine, whose bytes are the call graph's.
        let extra = write(&root, "src/extra.rs", "pub fn other() {}\n");
        ctx.data_note_paths(&[extra]);
        let checkout = ctx.data_checkout().unwrap();
        checkout.checkout().refresh_callgraph().unwrap();
        let edited = ctx.memory_root_snapshot();
        let edited_shared = edited.data_layer.clone().unwrap();
        assert_eq!(edited_shared.counts["changed_paths"], 1);
        assert!(edited_shared.counts["text_overlay_bytes"] > 0);
        assert!(edited_shared.counts["search_bytes"] >= edited_shared.counts["text_overlay_bytes"]);
        assert_eq!(edited_shared.counts["callgraph_engine_started"], 1);
        let engine = edited_shared.counts["callgraph_engine_bytes"];
        assert!(engine > 0);
        assert!(edited_shared.counts["callgraph_bytes"] > engine);
        let (edited_row, edited_planes) = census_row(&edited);
        assert_eq!(edited_row["attributed_bytes"], json!(edited_planes));
        assert_eq!(edited.attributed_bytes, edited_planes);

        // Releasing the idle checkout drops its share.
        drop(checkout);
        settled(&ctx);
        let started = Instant::now();
        while !ctx.try_release_idle_checkout_views() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the idle release never ran"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let released = ctx.memory_root_snapshot();
        assert_eq!(
            released.data_layer.as_ref().unwrap().counts["bound_checkouts"],
            0
        );
        assert_eq!(
            released.data_layer.as_ref().unwrap().estimated_bytes,
            Some(0)
        );
        for plane in [&released.trigram, &released.callgraph, &released.semantic] {
            assert_eq!(data_layer_bytes(plane), Some(0));
        }
        assert!(
            released.attributed_bytes
                <= edited.attributed_bytes
                    - edited_shared.estimated_bytes.unwrap()
                    - edited_shared.counts["search_bytes"]
                    - edited_shared.counts["callgraph_bytes"],
            "{} after release, {} before",
            released.attributed_bytes,
            edited.attributed_bytes
        );
    }

    /// Once meaning search has the root's semantic configuration, its
    /// plane, not today's index, is the root's `semantic_index` in health,
    /// as it is on the status tool.
    #[test]
    fn health_reports_the_meaning_plane_once_it_has_a_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = context(&root, storage.path());
        let checkout = ctx.data_checkout().unwrap();
        settled(&ctx);
        let starting = Some(crate::data_layer::meaning::STARTING_STAGE.to_string());
        let before = ctx.try_health_snapshot(&root).semantic_index.unwrap();
        assert_ne!(before.stage, starting, "today's index reports until then");

        checkout.configure_meaning_without_fill_for_test(Some(&ctx.config().semantic));
        let health = ctx.try_health_snapshot(&root).semantic_index.unwrap();
        assert_eq!(health.status, BUILDING);
        assert_eq!(health.stage, starting);
        assert_eq!(
            health.fill, None,
            "no fill is running and nothing is counted yet"
        );
        let payload = ctx.build_status_snapshot();
        assert_eq!(payload["semantic_index"]["stage"], json!(starting));
    }

    /// Health carries the meaning plane's fill in the shape it gives
    /// today's index, and only the counts that say something.
    #[test]
    fn health_reports_the_meaning_fill_and_its_failures() {
        use crate::data_layer::meaning::FillTimes;
        let coverage = MeaningCoverage {
            files: 40,
            files_embedded: 30,
            files_too_large: 1,
            files_failed: 2,
            files_pending: 7,
            files_past_cap: 0,
            chunks: 300,
            chunks_embedded: 280,
            chunks_failed: 3,
        };
        let filling = MeaningReport {
            plane: MeaningPlane::Serving {
                complete: false,
                coverage,
                error: Some("embedder timed out".into()),
            },
            filling: true,
            waiting: true,
            times: FillTimes {
                started_at_ms: Some(1_000),
                stored_at_ms: Some(2_000),
            },
        };
        let health = serde_json::to_value(filling.health()).unwrap();
        assert_eq!(
            health,
            json!({
                "status": "building",
                "reason": "embedder timed out",
                "stage": "waiting_for_another_fill",
                "embedded_chunks": 280,
                "total_chunks": 300,
                "files": 40,
                "files_embedded": 30,
                "files_pending": 7,
                "files_too_large": 1,
                "files_failed": 2,
                "chunks_failed": 3,
                "filling": true,
                "since_ms": 1_000,
                "last_progress_at_ms": 2_000,
            })
        );
        // The status tool keeps its full shape.
        let status = filling.status_json();
        assert_eq!(status["status"], "building");
        assert_eq!(status["stage"], "waiting_for_another_fill");
        assert_eq!(status["files_pending"], 7);
        assert_eq!(status["chunks_embedded"], 280);
        assert_eq!(status["error"], "embedder timed out");
        assert_eq!(status["filling"], true);
        assert_eq!(status["waiting_for_another_fill"], true);

        // Filled: ready, with only the failures it recorded.
        let ready = MeaningReport {
            plane: MeaningPlane::Serving {
                complete: true,
                coverage: MeaningCoverage {
                    files_pending: 0,
                    files_failed: 0,
                    ..coverage
                },
                error: None,
            },
            filling: false,
            waiting: false,
            times: FillTimes {
                started_at_ms: None,
                stored_at_ms: Some(2_000),
            },
        };
        // Only a building plane is dated.
        assert_eq!(
            serde_json::to_value(ready.health()).unwrap(),
            json!({"status": "ready", "chunks_failed": 3})
        );

        let failed = MeaningReport {
            plane: MeaningPlane::Failed("embedding model: no backend".into()),
            filling: false,
            waiting: false,
            times: FillTimes::default(),
        };
        assert_eq!(
            serde_json::to_value(failed.health()).unwrap(),
            json!({"status": "degraded", "reason": "embedding model: no backend"})
        );
        let starting = MeaningReport {
            plane: MeaningPlane::Building {
                stage: crate::data_layer::meaning::STARTING_STAGE.into(),
                files: None,
                entries_done: None,
                entries_total: None,
            },
            filling: true,
            waiting: false,
            times: FillTimes {
                started_at_ms: Some(3_000),
                stored_at_ms: None,
            },
        };
        assert_eq!(
            serde_json::to_value(starting.health()).unwrap(),
            json!({"status": "building", "stage": "loading_embedding_model", "filling": true,
                "since_ms": 3_000})
        );
    }

    #[test]
    fn a_checkout_estimate_splits_the_planes_and_names_what_it_left_out() {
        let live = LiveMemory {
            state_paths: 4,
            shared_bytes: 100,
            text_bytes: 50,
            text_overlay_paths: 1,
            text_overlay_bytes: 20,
            callgraph_bytes: 70,
            changes: 1,
            pending_callgraph_paths: 1,
            refreshed_files: 2,
            dead_code_files: Some(3),
        };
        let memory = checkout_estimate(
            CheckoutMemory {
                live: Some(live),
                engine: EngineMemory::Running {
                    parses: 4,
                    resolutions: 2,
                    state_bytes: 10,
                    bytes: 30,
                },
            },
            Some(40),
            0,
        );
        assert_eq!(
            (memory.text, memory.callgraph, memory.meaning),
            (50, 100, 40)
        );
        let shared = &memory.shared;
        assert_eq!(shared.estimated_bytes, Some(110));
        assert_eq!(shared.bytes_status, "estimated");
        assert!(shared.not_estimated.is_empty());
        assert_eq!(shared.counts["search_bytes"], 50);
        assert_eq!(shared.counts["callgraph_bytes"], 100);
        assert_eq!(shared.counts["callgraph_engine_bytes"], 30);
        assert_eq!(shared.counts["semantic_bytes"], 40);
        assert_eq!(shared.counts["callgraph_engine_parses"], 4);
        assert_eq!(shared.counts["dead_code_snapshot_files"], 3);

        // Folding adds each share to its plane; the shared part stays.
        let mut search = MemoryEstimate::estimated(5);
        let mut callgraph = MemoryEstimate::busy();
        let mut semantic = MemoryEstimate::estimated(0);
        let folded = memory.fold_into(&mut search, &mut callgraph, &mut semantic);
        assert_eq!(folded.estimated_bytes, Some(110));
        assert_eq!(search.estimated_bytes, Some(55));
        assert_eq!(semantic.estimated_bytes, Some(40));
        // A plane today's store could not read still counts the data
        // layer's share, and says it is partial.
        assert_eq!(callgraph.estimated_bytes, Some(100));
        assert_eq!(callgraph.bytes_status, "partial");
        assert_eq!(callgraph.status, "busy");

        let partial = checkout_estimate(
            CheckoutMemory {
                live: Some(live),
                engine: EngineMemory::Busy,
            },
            None,
            2,
        );
        assert_eq!(partial.shared.bytes_status, "partial");
        assert_eq!(
            partial.shared.not_estimated,
            [
                "callgraph_engine_busy",
                "meaning_plane_busy",
                "locked_parts"
            ]
        );
        assert_eq!(partial.shared.counts["locked_parts"], 2);

        let busy = checkout_estimate(
            CheckoutMemory {
                live: None,
                engine: EngineMemory::Busy,
            },
            Some(0),
            0,
        );
        assert_eq!(busy.shared.status, "busy");
        assert_eq!(busy.shared.not_estimated, ["checkout_state_busy"]);
        assert_eq!((busy.text, busy.callgraph, busy.meaning), (0, 0, 0));
    }

    #[test]
    fn warm_binds_and_index_publishes_once() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = context(&root, storage.path());

        // `aft index` builds in the foreground, then finds the store current.
        let all = crate::data_layer::checkout::Planes::ALL;
        let first = index_root(&root, storage.path(), all).unwrap();
        assert_eq!(
            first,
            IndexOutcome {
                files: 3,
                parsed_files: 2,
                unparsed_files: 0,
                published: true
            }
        );
        let again = index_root(&root, storage.path(), all).unwrap();
        assert!(!again.published, "nothing changed, nothing published");
        write(&root, "src/extra.rs", "pub fn other() {}\n");
        let changed = index_root(&root, storage.path(), all).unwrap();
        assert_eq!((changed.files, changed.published), (4, true));

        // A standing root that asks only for text search builds no call
        // graph, in a store of its own.
        let text_storage = tempfile::tempdir().unwrap();
        let text_only = index_root(
            &root,
            text_storage.path(),
            crate::data_layer::standing::planes(&[crate::config::IndexKind::Search]),
        )
        .unwrap();
        assert_eq!(
            (text_only.files, text_only.parsed_files, text_only.published),
            (4, 0, true)
        );

        // `aft warmup` binds; the publication `aft index` left serves at once.

        let started = Instant::now();
        loop {
            match warm(&ctx) {
                (WarmState::Ready, WarmState::Ready) => break,
                (WarmState::Failed(error), _) | (_, WarmState::Failed(error)) => {
                    panic!("{error}")
                }
                pending => assert!(
                    started.elapsed() < Duration::from_secs(60),
                    "never warmed: {pending:?}"
                ),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(report(&ctx).unwrap().files, Some(4));
    }

    /// A segment a newer build wrote into this root's family store, named
    /// by a manifest this build reads: the bind refuses it by name, and
    /// explicit status, status pushes and health report the refusal as
    /// today's stores report theirs.
    #[test]
    fn a_store_file_a_newer_build_wrote_is_refused_on_every_status_surface() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = context(&root, storage.path());
        assert!(ctx.data_checkout().is_some());
        settled(&ctx);
        assert!(ctx.try_release_idle_checkout_views());

        // Every segment on disk names segment format version 99, as a newer
        // build would write it; the length the manifest published holds.
        let families = std::fs::read_dir(storage.path().join("data"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(families.len(), 1, "{families:?}");
        let mut segments = std::fs::read_dir(families[0].join("segments"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        segments.sort();
        let mut planted = Vec::new();
        for path in &segments {
            let mut bytes = std::fs::read(path).unwrap();
            bytes[8..10].copy_from_slice(&99u16.to_le_bytes());
            std::fs::remove_file(path).unwrap();
            std::fs::write(path, &bytes).unwrap();
            planted.push(bytes);
        }

        assert!(ctx.data_checkout().is_none(), "the bind is refused");
        let Some(DataCheckoutProbe::BindFailed(error)) = ctx.probe_data_checkout() else {
            panic!("the bind failure is kept");
        };
        let refusal = error.refusal().expect("kept typed").clone();
        assert!(segments.contains(&refusal.path), "{refusal:?}");
        assert_eq!(
            (refusal.file, refusal.found, refusal.supported),
            (aft_store::family::FileKind::Segment, 99, 1)
        );
        let message = format!(
            "storage_requires_newer_reader: index_store at {} has format version 99; this build reads up to 1",
            refusal.path.display()
        );
        assert_eq!(error.to_string(), message);
        let entry = json!({
            "code": "storage_requires_newer_reader",
            "store": "index_store",
            "path": refusal.path.display().to_string(),
            "found": 99,
            "supported": 1,
            "message": message,
        });
        let reason = json!("storage_requires_newer_reader:index_store");

        let explicit = ctx.build_status_snapshot();
        assert_eq!(explicit["degraded"], true);
        assert!(
            explicit["degraded_reasons"]
                .as_array()
                .unwrap()
                .contains(&reason),
            "{explicit:#}"
        );
        assert_eq!(explicit["storage_refusals"], json!([entry]));
        for plane in ["search_index", "callgraph_store"] {
            assert_eq!(explicit[plane]["status"], UNAVAILABLE, "{plane}");
            assert_eq!(
                explicit[plane]["reason"],
                reason::REQUIRES_NEWER_READER,
                "{plane}"
            );
        }
        assert_eq!(explicit["data_store"]["error"], message);

        let pushed = ctx.try_build_status_push_snapshot().unwrap();
        assert_eq!(pushed["degraded"], true);
        assert!(
            pushed["degraded_reasons"]
                .as_array()
                .unwrap()
                .contains(&reason),
            "{pushed:#}"
        );
        assert_eq!(pushed["storage_refusals"], json!([entry]));
        assert_eq!(pushed["storage_refusals_total"], 1);
        assert_eq!(pushed["storage_refusals_omitted"], 0);

        let health = ctx.try_health_snapshot(&root).data_store.unwrap();
        assert_eq!(health.reason, Some(reason::REQUIRES_NEWER_READER));
        assert_eq!(health.error.as_deref(), Some(message.as_str()));

        // Nothing was rebuilt over the refused segments or reclaimed.
        let mut after = std::fs::read_dir(families[0].join("segments"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        after.sort();
        assert_eq!(after, segments);
        for (path, bytes) in segments.iter().zip(&planted) {
            assert_eq!(&std::fs::read(path).unwrap(), bytes, "{}", path.display());
        }
    }
}
