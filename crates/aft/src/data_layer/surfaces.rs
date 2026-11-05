//! The data layer on AFT's status surfaces.
//!
//! With the data layer on, the status tool and its pushes, daemon health,
//! the memory census, zoom's call-graph status, and the `aft warmup` and
//! `aft index` commands describe the root's checkout in the data layer for
//! the planes it serves: text (grep and glob) and the call graph. Meaning
//! search keeps reporting today's semantic index until it moves too. With
//! the data layer off, nothing here runs and every surface reports today's
//! stores.
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

use super::checkout::{CheckoutMemory, CheckoutStatus, EngineMemory, PlaneStatus};
use super::runtime::{DataCheckout, DataFailure};
use crate::context::AppContext;
use crate::memory::MemoryEstimate;

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
    BindFailed(String),
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
        let reason = if crate::view_storage_reserve::is_message(&failure.error) {
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
    /// The family store's folder, for its disk use.
    pub(crate) family_dir: Option<PathBuf>,
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
            family_dir: None,
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
pub(crate) fn report(ctx: &AppContext) -> Option<DataStoreReport> {
    Some(match ctx.probe_data_checkout()? {
        DataCheckoutProbe::NotBound => {
            DataStoreReport::both(PlaneReport::because(OFF, reason::NOT_BOUND, None))
        }
        DataCheckoutProbe::Binding => DataStoreReport::both(PlaneReport::of(BUILDING)),
        DataCheckoutProbe::BindFailed(error) => DataStoreReport::both(PlaneReport::because(
            UNAVAILABLE,
            reason::BIND_FAILED,
            Some(error),
        )),
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

/// How a bound checkout stands.
pub(crate) fn report_of(checkout: &DataCheckout) -> DataStoreReport {
    let status = checkout.checkout().status();
    let files = checkout.view().map(|view| view.state().paths().len());
    report_from(
        &status,
        files,
        checkout.last_failure(),
        Some(checkout.checkout().family_dir().to_path_buf()),
    )
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
        failure,
        family_dir,
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

/// The per-root `data_layer` memory estimate: what the root's checkout
/// holds beyond the shared block cache, which the process counts once.
/// `None` when the data layer is off.
pub(crate) fn memory_estimate(ctx: &AppContext) -> Option<MemoryEstimate> {
    Some(match ctx.probe_data_checkout()? {
        // Nothing is held for a checkout that is not bound.
        DataCheckoutProbe::NotBound
        | DataCheckoutProbe::BindFailed(_)
        | DataCheckoutProbe::Withheld(_) => {
            MemoryEstimate::estimated(0).count("bound_checkouts", 0)
        }
        DataCheckoutProbe::Binding => MemoryEstimate::busy().gap("checkout_binding"),
        DataCheckoutProbe::Bound(checkout) => checkout_estimate(checkout.checkout().memory()),
    })
}

/// A bound checkout's memory as a census estimate. The bytes cover its
/// checkout states and its text overlay; what it holds without a byte
/// figure yet is named as a gap, so the estimate is partial, never a
/// silent zero.
pub(crate) fn checkout_estimate(memory: CheckoutMemory) -> MemoryEstimate {
    let mut estimate = match memory.live {
        Some(live) => {
            let mut estimate =
                MemoryEstimate::partial(live.state_bytes.saturating_add(live.text_overlay_bytes))
                    .count("state_paths", live.state_paths)
                    .count_u64("state_bytes", live.state_bytes)
                    .count("text_overlay_paths", live.text_overlay_paths)
                    .count_u64("text_overlay_bytes", live.text_overlay_bytes)
                    .count("changed_paths", live.changes)
                    .count("pending_callgraph_paths", live.pending_callgraph_paths)
                    .count("refreshed_callgraph_files", live.refreshed_files);
            if live.state_paths > 0 {
                // The publication's per-path text locations and its decoded
                // call-graph slices.
                estimate = estimate
                    .gap("published_text_membership")
                    .gap("published_call_graph_slices");
            }
            if live.changes > 0 {
                estimate = estimate.gap("changed_file_parses");
            }
            if live.refreshed_files > 0 {
                estimate = estimate.gap("refreshed_call_graph");
            }
            if let Some(files) = live.dead_code_files {
                // Shared with aft_inspect, which may hold the same snapshot.
                estimate = estimate
                    .count("dead_code_snapshot_files", files)
                    .gap("dead_code_snapshot");
            }
            estimate
        }
        None => MemoryEstimate::busy().gap("checkout_state_busy"),
    };
    estimate = estimate.count("bound_checkouts", 1);
    match memory.engine {
        EngineMemory::NotStarted => estimate.count("callgraph_engine_started", 0),
        EngineMemory::Busy => estimate
            .count("callgraph_engine_started", 1)
            .gap("callgraph_engine_busy"),
        EngineMemory::Running {
            parses,
            resolutions,
        } => estimate
            .count("callgraph_engine_started", 1)
            .count("callgraph_engine_parses", parses)
            .count("callgraph_engine_resolutions", resolutions)
            .gap("callgraph_engine"),
    }
}

/// The process's shared block cache as a census estimate, counted once for
/// every checkout; `None` while the data layer is off or no checkout of
/// this process has bound.
pub(crate) fn block_cache_estimate() -> Option<MemoryEstimate> {
    if !super::runtime::enabled() {
        return None;
    }
    let stats = super::runtime::block_cache_stats()?;
    Some(
        MemoryEstimate::estimated(u64::try_from(stats.resident_bytes).unwrap_or(u64::MAX))
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

/// Whether this process runs the data layer (`AFT_DATA_LAYER=1`).
#[must_use]
pub fn enabled() -> bool {
    super::runtime::enabled()
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
    /// Builds that may stand down for another process's publication before
    /// this one gives up.
    const ATTEMPTS: usize = 3;
    let checkout = super::runtime::bind_foreground(root, storage, planes)
        .map_err(|error| error.to_string())?;
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
        if current(&checkout) {
            break;
        }
        let report = checkout.build().map_err(|error| error.to_string())?;
        published |= !report.superseded;
    }
    let view = checkout
        .view()
        .filter(|_| current(&checkout))
        .ok_or_else(|| {
            "other AFT processes kept publishing this checkout while it was built".to_string()
        })?;
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
        crate::data_layer::runtime::force_on_for_test();
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
        crate::data_layer::runtime::force_on_for_test();
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
        crate::data_layer::runtime::force_on_for_test();
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
        crate::data_layer::runtime::force_on_for_test();
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
        crate::data_layer::runtime::force_on_for_test();
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = context(&root, storage.path());

        // Not bound: nothing held, said so.
        let unbound = ctx.memory_root_snapshot().data_layer.unwrap();
        assert_eq!(unbound.estimated_bytes, Some(0));
        assert_eq!(unbound.counts["bound_checkouts"], 0);
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
        let root_memory = ctx.memory_root_snapshot();
        let data_layer = root_memory.data_layer.clone().unwrap();
        assert_eq!(data_layer.status, "ready");
        assert_eq!(data_layer.bytes_status, "partial");
        assert!(data_layer.estimated_bytes.unwrap() > 0);
        assert_eq!(data_layer.counts["bound_checkouts"], 1);
        assert_eq!(data_layer.counts["state_paths"], 3);
        for gap in ["published_text_membership", "published_call_graph_slices"] {
            assert!(
                data_layer.not_estimated.iter().any(|named| named == gap),
                "{gap}"
            );
        }
        assert!(
            root_memory.attributed_bytes >= data_layer.estimated_bytes.unwrap(),
            "the root's total includes the data layer"
        );

        let snapshot = crate::memory::MemorySnapshot::new(
            "ready",
            [(root.display().to_string(), root_memory)].into(),
        );
        let cache = snapshot.data_layer_block_cache.clone().unwrap();
        assert!(cache.counts["budget_bytes"] > 0);
        assert_eq!(
            snapshot.process.data_layer_block_cache_bytes,
            cache.estimated_bytes
        );
        assert!(
            snapshot.process.total_attributed_bytes
                >= cache.estimated_bytes.unwrap() + data_layer.estimated_bytes.unwrap()
        );
        let census = crate::commands::memory_census::render_memory_census(&snapshot, None);
        let row = &census["roots"][root.display().to_string()];
        assert_eq!(
            row["planes"]["data_layer"],
            json!(data_layer.estimated_bytes.unwrap())
        );
        assert_eq!(row["data_layer"]["bytes_status"], "partial");
        assert_eq!(
            census["process"]["data_layer_block_cache_bytes"],
            json!(cache.estimated_bytes)
        );

        // An edit adds its overlay and names the parse it holds.
        let extra = write(&root, "src/extra.rs", "pub fn other() {}\n");
        ctx.data_note_paths(&[extra]);
        let edited = ctx.memory_root_snapshot().data_layer.unwrap();
        assert_eq!(edited.counts["changed_paths"], 1);
        assert!(edited.counts["text_overlay_bytes"] > 0);
        assert!(edited
            .not_estimated
            .iter()
            .any(|gap| gap == "changed_file_parses"));
    }

    #[test]
    fn a_checkout_estimate_names_every_part_it_cannot_size() {
        let estimate = checkout_estimate(CheckoutMemory {
            live: Some(LiveMemory {
                state_paths: 4,
                state_bytes: 100,
                text_overlay_paths: 1,
                text_overlay_bytes: 20,
                changes: 1,
                pending_callgraph_paths: 1,
                refreshed_files: 2,
                dead_code_files: Some(3),
            }),
            engine: EngineMemory::Running {
                parses: 4,
                resolutions: 2,
            },
        });
        assert_eq!(estimate.estimated_bytes, Some(120));
        assert_eq!(estimate.bytes_status, "partial");
        assert_eq!(
            estimate.not_estimated,
            [
                "published_text_membership",
                "published_call_graph_slices",
                "changed_file_parses",
                "refreshed_call_graph",
                "dead_code_snapshot",
                "callgraph_engine",
            ]
        );
        assert_eq!(estimate.counts["callgraph_engine_parses"], 4);
        assert_eq!(estimate.counts["dead_code_snapshot_files"], 3);

        let busy = checkout_estimate(CheckoutMemory {
            live: None,
            engine: EngineMemory::Busy,
        });
        assert_eq!(busy.status, "busy");
        assert_eq!(
            busy.not_estimated,
            ["checkout_state_busy", "callgraph_engine_busy"]
        );
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
        crate::data_layer::runtime::force_on_for_test();
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
}
