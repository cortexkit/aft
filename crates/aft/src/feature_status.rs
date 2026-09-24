//! Observed status of the three background indexes (trigram, semantic,
//! callgraph).
//!
//! Every surface that reports index state (the setup plan, doctor and the tool
//! consumers such as grep, aft_search, aft_callgraph, inspect and zoom) reads it
//! through [`observed_index_status`], so they agree on the effective state and
//! on the named cause when an enabled index cannot be used.
//!
//! Observation is passive: it only reads what the runtime already holds and
//! never starts, reloads or retries a build.

use serde_json::{json, Value};

use crate::context::{AppContext, SemanticIndexStatus};

/// One background index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IndexPlane {
    Trigram,
    Semantic,
    Callgraph,
}

impl IndexPlane {
    pub const ALL: [IndexPlane; 3] = [
        IndexPlane::Trigram,
        IndexPlane::Semantic,
        IndexPlane::Callgraph,
    ];

    /// The canonical config path of the plane's switch.
    pub const fn config_path(self) -> &'static str {
        match self {
            IndexPlane::Trigram => "indexes.trigram",
            IndexPlane::Semantic => "indexes.semantic",
            IndexPlane::Callgraph => "indexes.callgraph",
        }
    }

    /// Short plane name used as the key in consumer payloads.
    pub const fn as_str(self) -> &'static str {
        match self {
            IndexPlane::Trigram => "trigram",
            IndexPlane::Semantic => "semantic",
            IndexPlane::Callgraph => "callgraph",
        }
    }

    /// Whether the resolved configuration turns this index on.
    pub fn enabled_in(self, config: &crate::config::Config) -> bool {
        match self {
            IndexPlane::Trigram => config.indexes.trigram,
            IndexPlane::Semantic => config.indexes.semantic,
            IndexPlane::Callgraph => config.indexes.callgraph,
        }
    }
}

/// Effective feature state shared by setup, doctor and consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IndexEffective {
    /// Resolved configuration turns the index off.
    Off,
    /// Enabled and a build is in progress; not yet usable.
    Building,
    /// Enabled and usable.
    Ready,
    /// Enabled but not usable; the observation names the cause.
    Unavailable,
}

impl IndexEffective {
    pub const fn as_str(self) -> &'static str {
        match self {
            IndexEffective::Off => "off",
            IndexEffective::Building => "building",
            IndexEffective::Ready => "ready",
            IndexEffective::Unavailable => "unavailable",
        }
    }
}

/// Named causes carried in `unavailable_reason`.
pub mod cause {
    /// The index is enabled but this runtime has not observed it yet (no build
    /// has started, or the caller has no engine runtime at all).
    pub const RUNTIME_NOT_OBSERVED: &str = "runtime_not_observed";
    /// The project root is the user's home directory; heavy indexes never run
    /// there.
    pub const HOME_ROOT: &str = "home_root";
    /// The configured embedding backend is currently unreachable.
    pub const SEMANTIC_BACKEND_UNAVAILABLE: &str = "semantic_backend_unavailable";
    /// The local embedding backend needs an ONNX Runtime that AFT cannot
    /// provide on this platform, and no other backend is configured.
    pub const SEMANTIC_PLATFORM_UNSUPPORTED: &str = "semantic_platform_unsupported";
    /// The local embedding backend could not load an ONNX Runtime on a
    /// platform where one can be installed.
    pub const ONNX_RUNTIME_UNAVAILABLE: &str = "onnx_runtime_unavailable";
    /// The semantic build failed for another reason.
    pub const SEMANTIC_BUILD_FAILED: &str = "semantic_build_failed";
    /// The callgraph builder was suspended after repeated build deaths.
    pub const BUILD_SUSPENDED: &str = "build_suspended";
    /// The callgraph build was refused for this configure generation.
    pub const CALLGRAPH_BUILD_DENIED: &str = "callgraph_build_denied";
    /// A read-only checkout (a linked worktree) shares a callgraph store that
    /// the main checkout has not built yet.
    pub const READ_ONLY_STORE_NOT_BUILT: &str = "read_only_store_not_built";
}

/// Observed state of one index plane: the effective state plus, only when
/// unavailable, the named cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexObservation {
    pub effective: IndexEffective,
    pub unavailable_reason: Option<String>,
}

impl IndexObservation {
    pub fn off() -> Self {
        Self {
            effective: IndexEffective::Off,
            unavailable_reason: None,
        }
    }

    pub fn ready() -> Self {
        Self {
            effective: IndexEffective::Ready,
            unavailable_reason: None,
        }
    }

    pub fn building() -> Self {
        Self {
            effective: IndexEffective::Building,
            unavailable_reason: None,
        }
    }

    pub fn unavailable(cause: impl Into<String>) -> Self {
        Self {
            effective: IndexEffective::Unavailable,
            unavailable_reason: Some(cause.into()),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.effective == IndexEffective::Ready
    }

    /// The `{status, reason}` object consumers embed in their responses. In
    /// consumer payloads `reason` carries only the unavailable cause (null for
    /// off, building and ready); the setup plan's derivation reason is a
    /// separate field that consumers do not expose.
    pub fn consumer_json(&self) -> Value {
        json!({
            "status": self.effective.as_str(),
            "reason": self.unavailable_reason,
        })
    }
}

/// Observation for a caller without an engine runtime (standalone CLI): a
/// resolved-off index is off, an enabled one is `runtime_not_observed`.
pub fn unobserved_index_status(enabled: bool) -> IndexObservation {
    if enabled {
        IndexObservation::unavailable(cause::RUNTIME_NOT_OBSERVED)
    } else {
        IndexObservation::off()
    }
}

/// Current observed status of `plane` in this engine runtime.
///
/// A resolved-off index is `off` regardless of backend support or whether it
/// was ever observed. A HOME root is the one exception: configure turns its
/// indexes off internally, so it reports `unavailable` with `home_root`. An enabled index is `ready` or `building` when the
/// runtime holds that state, otherwise `unavailable` with a named cause
/// (`runtime_not_observed` when nothing has been observed yet).
pub fn observed_index_status(ctx: &AppContext, plane: IndexPlane) -> IndexObservation {
    // Configure forces every index off for a HOME root (degraded mode). That
    // is not the user's resolved choice, so report it as unavailable with its
    // cause rather than as configured off.
    if ctx.is_home_root() {
        return IndexObservation::unavailable(cause::HOME_ROOT);
    }
    if !plane.enabled_in(&ctx.config()) {
        return IndexObservation::off();
    }
    match plane {
        IndexPlane::Trigram => observe_trigram(ctx),
        IndexPlane::Semantic => observe_semantic(ctx),
        IndexPlane::Callgraph => observe_callgraph(ctx),
    }
}

fn observe_trigram(ctx: &AppContext) -> IndexObservation {
    let resident = {
        let guard = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.as_ref().map(|index| index.ready)
    };
    match resident {
        Some(true) => IndexObservation::ready(),
        Some(false) => IndexObservation::building(),
        None => {
            let loading = ctx
                .search_index_rx()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
            if loading {
                IndexObservation::building()
            } else {
                IndexObservation::unavailable(cause::RUNTIME_NOT_OBSERVED)
            }
        }
    }
}

fn observe_semantic(ctx: &AppContext) -> IndexObservation {
    let status = ctx
        .semantic_index_status()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let SemanticIndexStatus::Failed(message) = &status {
        return IndexObservation::unavailable(semantic_failure_cause(ctx, message));
    }
    if !ctx.semantic_backend_health_snapshot().available {
        return IndexObservation::unavailable(cause::SEMANTIC_BACKEND_UNAVAILABLE);
    }
    let resident = ctx
        .semantic_index()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some();
    match status {
        SemanticIndexStatus::Ready { .. } if resident => IndexObservation::ready(),
        SemanticIndexStatus::Building { .. } => IndexObservation::building(),
        _ if ctx.semantic_index_rx().lock().is_some() => IndexObservation::building(),
        _ => IndexObservation::unavailable(cause::RUNTIME_NOT_OBSERVED),
    }
}

/// Classify a failed semantic build into a named cause.
pub fn semantic_failure_cause(ctx: &AppContext, message: &str) -> &'static str {
    let local_backend = matches!(
        ctx.config().semantic.backend,
        crate::config::SemanticBackend::Fastembed
    );
    if local_backend && crate::semantic_index::is_onnx_runtime_unavailable(message) {
        if local_semantic_platform_supported() {
            cause::ONNX_RUNTIME_UNAVAILABLE
        } else {
            cause::SEMANTIC_PLATFORM_UNSUPPORTED
        }
    } else {
        cause::SEMANTIC_BUILD_FAILED
    }
}

/// Whether AFT can provide an ONNX Runtime for the local embedding backend on
/// the platform this binary was built for. Mirrors the download table in
/// packages/aft-bridge/src/onnx-runtime.ts (`ORT_PLATFORM_MAP`, with Windows
/// arm64 served by the x64 runtime); a change there must be mirrored here.
pub const fn local_semantic_platform_supported() -> bool {
    cfg!(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "aarch64"),
    ))
}

fn observe_callgraph(ctx: &AppContext) -> IndexObservation {
    let resident = ctx
        .callgraph_store()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some();
    if resident && ctx.pending_callgraph_store_force_token().is_none() {
        return IndexObservation::ready();
    }
    if ctx.callgraph_store_build_suspension().is_some() {
        return IndexObservation::unavailable(cause::BUILD_SUSPENDED);
    }
    if ctx.callgraph_store_build_denial().is_some() {
        return IndexObservation::unavailable(cause::CALLGRAPH_BUILD_DENIED);
    }
    if ctx.callgraph_store_rx().lock().is_some()
        || ctx.pending_callgraph_store_force_token().is_some()
    {
        return IndexObservation::building();
    }
    IndexObservation::unavailable(cause::RUNTIME_NOT_OBSERVED)
}

/// Translate the result of a callgraph store request into the plane's
/// observation, so a consumer that already asked for the store reports the
/// same state it acted on.
pub fn callgraph_access_observation(
    ctx: &AppContext,
    access: &crate::context::CallgraphStoreAccess,
) -> IndexObservation {
    use crate::context::CallgraphStoreAccess;
    match access {
        CallgraphStoreAccess::Off => IndexObservation::off(),
        CallgraphStoreAccess::Ready(_) => IndexObservation::ready(),
        CallgraphStoreAccess::Building => IndexObservation::building(),
        CallgraphStoreAccess::Suspended(_) => IndexObservation::unavailable(cause::BUILD_SUSPENDED),
        CallgraphStoreAccess::Unavailable | CallgraphStoreAccess::Error(_) => {
            match observed_index_status(ctx, IndexPlane::Callgraph) {
                observation if observation.effective == IndexEffective::Unavailable => observation,
                _ => IndexObservation::unavailable(cause::RUNTIME_NOT_OBSERVED),
            }
        }
    }
}

#[cfg(test)]
#[path = "feature_status/consumer_fixture_tests.rs"]
mod consumer_fixture_tests;
