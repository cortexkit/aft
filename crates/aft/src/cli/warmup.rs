use aft::callgraph_store::CallGraphStore;
use aft::config::Config;
use aft::context::{AppContext, CallgraphStoreAccess, SemanticIndexEvent, SemanticIndexStatus};
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use serde_json::json;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_MS: u64 = 600_000;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub fn run(args: Vec<OsString>) -> Result<(), WarmupError> {
    let args = WarmupArgs::parse(args)?;
    if args.help {
        print_usage();
        return Ok(());
    }

    let root = args
        .root
        .ok_or_else(|| WarmupError::usage("missing required --root <path>"))?;
    if !root.is_absolute() {
        return Err(WarmupError::usage(format!(
            "--root must be an absolute path: {}",
            root.display()
        )));
    }
    if !root.is_dir() {
        return Err(WarmupError::usage(format!(
            "--root is not a directory: {}",
            root.display()
        )));
    }

    let storage_dir = warmup_storage_dir();
    if !storage_dir.is_absolute() {
        return Err(WarmupError::usage(format!(
            "AFT_STORAGE_DIR must be absolute when set: {}",
            storage_dir.display()
        )));
    }
    if std::env::var_os("FASTEMBED_CACHE_DIR").is_none() {
        std::env::set_var(
            "FASTEMBED_CACHE_DIR",
            storage_dir.join("semantic").join("models"),
        );
    }

    // Resolve ONNX Runtime from the plugin-managed cache so `aft warmup`
    // can build the semantic index without the plugin setting ORT_DYLIB_PATH.
    if args.areas.semantic {
        try_set_ort_dylib_path(&storage_dir);
    }

    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
    configure(&ctx, &root, &storage_dir, args.areas, args.force)?;

    // The callgraph store has no configure flag; building is triggered by
    // calling `callgraph_store_for_ops` once (warm-opens an existing store, or
    // kicks a background cold build). `None` => poll for readiness; `Some(_)`
    // => a terminal state captured at trigger (worktree/unavailable or error).
    let callgraph_override = if args.areas.callgraph {
        trigger_callgraph_warm(&ctx)
    } else {
        Some(SubsystemState::Disabled)
    };

    wait_until_ready(
        &ctx,
        args.areas,
        callgraph_override,
        args.timeout_ms,
        args.quiet,
    )
}

#[derive(Debug)]
pub struct WarmupError {
    message: String,
    exit_code: i32,
}

impl WarmupError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 2,
        }
    }

    fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 1,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

impl fmt::Display for WarmupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WarmupError {}

/// Which subsystems to warm. Default is all three; `--only` narrows the set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WarmupAreas {
    search: bool,
    semantic: bool,
    callgraph: bool,
}

impl WarmupAreas {
    /// Default: warm everything.
    fn all() -> Self {
        Self {
            search: true,
            semantic: true,
            callgraph: true,
        }
    }

    /// Parse a comma-separated `--only` list (e.g. `search,callgraph`).
    fn parse_only(value: &str) -> Result<Self, WarmupError> {
        let mut areas = Self {
            search: false,
            semantic: false,
            callgraph: false,
        };
        let mut any = false;
        for raw in value.split(',') {
            let name = raw.trim();
            if name.is_empty() {
                continue;
            }
            match name {
                "search" => areas.search = true,
                "semantic" => areas.semantic = true,
                "callgraph" => areas.callgraph = true,
                other => {
                    return Err(WarmupError::usage(format!(
                        "--only: unknown area '{other}' (expected search, semantic, or callgraph)"
                    )));
                }
            }
            any = true;
        }
        if !any {
            return Err(WarmupError::usage(
                "--only requires at least one of: search, semantic, callgraph",
            ));
        }
        Ok(areas)
    }
}

#[derive(Debug)]
struct WarmupArgs {
    root: Option<PathBuf>,
    timeout_ms: u64,
    quiet: bool,
    help: bool,
    /// Subsystems to warm (default: all).
    areas: WarmupAreas,
    /// Bypass file-count caps (semantic `semantic.max_files` and the search
    /// index limit) so a very large repo is fully indexed. Intended
    /// for benchmarking/measuring the worst case, not normal warmup.
    force: bool,
}

impl WarmupArgs {
    fn parse(args: Vec<OsString>) -> Result<Self, WarmupError> {
        let mut parsed = Self {
            root: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            quiet: false,
            help: false,
            areas: WarmupAreas::all(),
            force: false,
        };

        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            let Some(arg) = arg.to_str() else {
                return Err(WarmupError::usage("arguments must be valid UTF-8"));
            };
            match arg {
                "--root" => {
                    let value = next_value(&mut iter, "--root")?;
                    parsed.root = Some(PathBuf::from(value));
                }
                "--timeout" => {
                    let value = next_value(&mut iter, "--timeout")?;
                    parsed.timeout_ms = value.parse::<u64>().map_err(|_| {
                        WarmupError::usage(format!("--timeout must be milliseconds, got {value}"))
                    })?;
                    if parsed.timeout_ms == 0 {
                        return Err(WarmupError::usage("--timeout must be greater than 0"));
                    }
                }
                "--only" => {
                    let value = next_value(&mut iter, "--only")?;
                    parsed.areas = WarmupAreas::parse_only(&value)?;
                }
                "--quiet" => parsed.quiet = true,
                "--force" => parsed.force = true,
                "--help" | "-h" => parsed.help = true,
                other => {
                    return Err(WarmupError::usage(format!(
                        "unknown warmup argument: {other}"
                    )));
                }
            }
        }

        Ok(parsed)
    }
}

fn next_value(
    iter: &mut impl Iterator<Item = OsString>,
    flag: &str,
) -> Result<String, WarmupError> {
    let value = iter
        .next()
        .ok_or_else(|| WarmupError::usage(format!("{flag} requires a value")))?;
    value
        .into_string()
        .map_err(|_| WarmupError::usage(format!("{flag} requires a valid UTF-8 value")))
}

fn print_usage() {
    println!(
        "aft warmup --root <absolute-path> [--only <areas>] [--timeout <ms>] [--quiet] [--force]"
    );
    println!(
        "  --only   comma-separated subset to warm: search, semantic, callgraph (default: all)"
    );
    println!(
        "  --force  bypass file-count caps (callgraph + semantic) to fully index a large repo"
    );
}

fn warmup_storage_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("AFT_STORAGE_DIR") {
        return PathBuf::from(value);
    }
    // Same CortexKit shared data root the plugins inject - warming here must
    // land in the storage universe real sessions will read from.
    aft::bash_background::storage_dir(None)
}

/// Set `ORT_DYLIB_PATH` from a cached ONNX Runtime library if not already set.
fn try_set_ort_dylib_path(storage_dir: &std::path::Path) {
    if std::env::var_os("ORT_DYLIB_PATH").is_some() {
        return;
    }
    let lib_name = if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else {
        "libonnxruntime.so"
    };
    let ort_dir = storage_dir.join("onnxruntime").join("1.24.4");
    for candidate in [ort_dir.join(lib_name), ort_dir.join("lib").join(lib_name)] {
        if candidate.is_file() {
            std::env::set_var("ORT_DYLIB_PATH", &candidate);
            break;
        }
    }
}

/// Build the `configure` params for a warmup run.
///
/// P1 config relocation: core config (search_index/semantic_search) is now
/// resolved exclusively from `config: [{tier, source, doc}]` tiers — the flat
/// params are no longer read by handle_configure. A synthetic user-tier doc
/// carries the two flags so warmup actually enables the requested systems
/// (process-state params like storage_dir/_bypass_size_limits stay flat).
fn build_warmup_configure_params(
    root: &std::path::Path,
    storage_dir: &std::path::Path,
    areas: WarmupAreas,
    force: bool,
) -> serde_json::Value {
    let warmup_config_doc = json!({
        "search_index": areas.search,
        "semantic_search": areas.semantic,
        "callgraph_store": areas.callgraph,
    })
    .to_string();
    let mut params = json!({
        "project_root": root.display().to_string(),
        "harness": "opencode",
        "storage_dir": storage_dir.display().to_string(),
        "config": [{
            "tier": "user",
            "source": "<aft-warmup>",
            "doc": warmup_config_doc,
        }],
    });
    if force {
        // Lift file-count caps (semantic `semantic.max_files` and the
        // hardcoded search-index file limit) so a
        // very large repo is fully indexed for measurement. configure honors
        // this internal flag by raising the effective caps and skipping the
        // size-based auto-disable.
        params["_bypass_size_limits"] = json!(true);
    }
    params
}

fn configure(
    ctx: &AppContext,
    root: &std::path::Path,
    storage_dir: &std::path::Path,
    areas: WarmupAreas,
    force: bool,
) -> Result<(), WarmupError> {
    // The callgraph store has no configure flag of its own (it builds lazily on
    // first op); it's triggered separately after configure. search/semantic are
    // configure-gated, so warm only the requested ones.
    let params = build_warmup_configure_params(root, storage_dir, areas, force);
    let req = RawRequest {
        id: "warmup-configure".to_string(),
        command: "configure".to_string(),
        lsp_hints: None,
        session_id: Some("warmup".to_string()),
        params,
    };

    let response = aft::commands::configure::handle_configure(&req, ctx);
    if response.success {
        Ok(())
    } else {
        Err(WarmupError::runtime(format_response_error(
            "configure",
            response,
        )))
    }
}

fn wait_until_ready(
    ctx: &AppContext,
    areas: WarmupAreas,
    callgraph_override: Option<SubsystemState>,
    timeout_ms: u64,
    quiet: bool,
) -> Result<(), WarmupError> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut last_labels = WarmupLabels::default();
    loop {
        drain_search_index_events(ctx);
        drain_semantic_index_events(ctx);
        if areas.callgraph {
            drain_callgraph_store_events(ctx);
        }

        let snapshot = WarmupSnapshot::from_context(ctx, &callgraph_override);
        if !quiet {
            let labels = snapshot.labels();
            labels.print_transitions(&mut last_labels);
        }
        if snapshot.is_terminal() {
            if !quiet {
                println!("aft warmup: ready");
            }
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(WarmupError::runtime(format!(
                "aft warmup timed out after {timeout_ms}ms; pending: {}",
                snapshot.pending_summary()
            )));
        }

        thread::sleep(POLL_INTERVAL);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SubsystemState {
    Pending(String),
    Ready,
    Disabled,
    Failed(String),
}

impl SubsystemState {
    fn is_terminal(&self) -> bool {
        !matches!(self, Self::Pending(_))
    }

    fn label(&self) -> String {
        match self {
            Self::Pending(detail) => format!("building ({detail})"),
            Self::Ready => "ready".to_string(),
            Self::Disabled => "disabled".to_string(),
            Self::Failed(error) => format!("failed ({error})"),
        }
    }
}

struct WarmupSnapshot {
    search_index: SubsystemState,
    semantic_index: SubsystemState,
    symbol_cache: SubsystemState,
    callgraph_store: SubsystemState,
}

impl WarmupSnapshot {
    fn from_context(ctx: &AppContext, callgraph_override: &Option<SubsystemState>) -> Self {
        let search_index = search_index_state(ctx);
        let semantic_index = semantic_index_state(ctx);
        let symbol_cache = symbol_cache_state(&search_index);
        let callgraph_store = callgraph_store_state(ctx, callgraph_override);
        Self {
            search_index,
            semantic_index,
            symbol_cache,
            callgraph_store,
        }
    }

    fn is_terminal(&self) -> bool {
        self.search_index.is_terminal()
            && self.semantic_index.is_terminal()
            && self.symbol_cache.is_terminal()
            && self.callgraph_store.is_terminal()
    }

    fn labels(&self) -> WarmupLabels {
        WarmupLabels {
            search_index: self.search_index.label(),
            semantic_index: self.semantic_index.label(),
            symbol_cache: self.symbol_cache.label(),
            callgraph_store: self.callgraph_store.label(),
        }
    }

    fn pending_summary(&self) -> String {
        let mut pending = Vec::new();
        if let SubsystemState::Pending(detail) = &self.search_index {
            pending.push(format!("search_index={detail}"));
        }
        if let SubsystemState::Pending(detail) = &self.semantic_index {
            pending.push(format!("semantic_index={detail}"));
        }
        if let SubsystemState::Pending(detail) = &self.symbol_cache {
            pending.push(format!("symbol_cache={detail}"));
        }
        if let SubsystemState::Pending(detail) = &self.callgraph_store {
            pending.push(format!("callgraph_store={detail}"));
        }
        if pending.is_empty() {
            "none".to_string()
        } else {
            pending.join(", ")
        }
    }
}

#[derive(Default)]
struct WarmupLabels {
    search_index: String,
    semantic_index: String,
    symbol_cache: String,
    callgraph_store: String,
}

impl WarmupLabels {
    fn print_transitions(&self, previous: &mut Self) {
        print_transition(
            "search_index",
            &self.search_index,
            &mut previous.search_index,
        );
        print_transition(
            "semantic_index",
            &self.semantic_index,
            &mut previous.semantic_index,
        );
        print_transition(
            "symbol_cache",
            &self.symbol_cache,
            &mut previous.symbol_cache,
        );
        print_transition(
            "callgraph_store",
            &self.callgraph_store,
            &mut previous.callgraph_store,
        );
    }
}

fn print_transition(name: &str, current: &str, previous: &mut String) {
    if previous != current {
        println!("aft warmup: {name} {current}");
        *previous = current.to_string();
    }
}

fn search_index_state(ctx: &AppContext) -> SubsystemState {
    if !ctx.config().search_index {
        return SubsystemState::Disabled;
    }
    let index_ready = {
        let search_index = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        search_index.as_ref().is_some_and(|index| index.ready)
    };
    if index_ready {
        return SubsystemState::Ready;
    }
    let build_in_progress = {
        let search_index_rx = ctx
            .search_index_rx()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        search_index_rx.is_some()
    };
    if build_in_progress {
        SubsystemState::Pending("building".to_string())
    } else {
        SubsystemState::Pending("loading".to_string())
    }
}

fn semantic_index_state(ctx: &AppContext) -> SubsystemState {
    if !ctx.config().semantic_search {
        return SubsystemState::Disabled;
    }
    match ctx
        .semantic_index_status()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        SemanticIndexStatus::Disabled => SubsystemState::Disabled,
        SemanticIndexStatus::Ready { .. } => SubsystemState::Ready,
        SemanticIndexStatus::Failed(error) => SubsystemState::Failed(error),
        SemanticIndexStatus::Building {
            stage,
            files,
            entries_done,
            entries_total,
        } => {
            let mut detail = stage;
            if let Some(files) = files {
                detail.push_str(&format!(", files={files}"));
            }
            if let (Some(done), Some(total)) = (entries_done, entries_total) {
                detail.push_str(&format!(", entries={done}/{total}"));
            }
            SubsystemState::Pending(detail)
        }
    }
}

fn symbol_cache_state(search_index: &SubsystemState) -> SubsystemState {
    match search_index {
        SubsystemState::Pending(_) => {
            SubsystemState::Pending("waiting_for_search_index".to_string())
        }
        SubsystemState::Ready | SubsystemState::Disabled | SubsystemState::Failed(_) => {
            SubsystemState::Ready
        }
    }
}

/// Kick the callgraph-store build. `callgraph_store_for_ops` warm-opens an
/// existing on-disk store synchronously, or starts a background cold build and
/// returns `Building`. Returns `None` to mean "poll for readiness" (Ready or a
/// cold build in flight), or `Some(state)` for a terminal outcome that won't
/// change by polling (worktree/unconfigured = treated as ready/no-op, or a hard
/// build error).
fn trigger_callgraph_warm(ctx: &AppContext) -> Option<SubsystemState> {
    match ctx.callgraph_store_for_ops() {
        CallgraphStoreAccess::Ready(_) if ctx.callgraph_store_rx().lock().is_some() => None,
        CallgraphStoreAccess::Ready(_) => Some(SubsystemState::Ready),
        // Building (or just-started cold build) -> drive to completion via the
        // wait loop draining `callgraph_store_rx`.
        CallgraphStoreAccess::Building => None,
        // Read-only worktree or not configured: nothing to build here.
        CallgraphStoreAccess::Unavailable => Some(SubsystemState::Ready),
        CallgraphStoreAccess::Error(error) => Some(SubsystemState::Failed(error.to_string())),
    }
}

// These warmup drain copies are intentionally separate from `runtime_drain`: the
// warmup CLI is a one-shot with no live status consumer or mid-build edit
// stream, so it keeps leaner drains without status-emitter signals. Do not
// deduplicate them unless that behavior changes.
/// Install the finished callgraph store once the background cold build sends it
/// over `callgraph_store_rx`. Mirrors `main::drain_callgraph_store_events` but
/// without the status-emitter signal (warmup has no sidebar consumer).
fn drain_callgraph_store_events(ctx: &AppContext) {
    let (latest, disconnected) = {
        let rx_ref = ctx.callgraph_store_rx().lock();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };
        let mut latest = None;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(store) => latest = Some(store),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (latest, disconnected)
    };

    let mut installed = false;
    if let Some(store) = latest {
        let pending = ctx.take_pending_callgraph_store_paths();
        drop(store);
        if !pending.is_empty() {
            ctx.enqueue_callgraph_store_refresh(pending);
        }
        if let Some(project_root) = ctx.callgraph_project_root() {
            if let Ok(Some(store)) =
                CallGraphStore::open_readonly(ctx.callgraph_store_dir(), project_root)
            {
                *ctx.callgraph_store()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(std::sync::Arc::new(store));
                installed = true;
            }
        }
    }

    if disconnected || installed {
        *ctx.callgraph_store_rx().lock() = None;
        if disconnected && !installed {
            let _ = ctx.take_pending_callgraph_store_paths();
        }
    }
}

fn callgraph_store_state(
    ctx: &AppContext,
    override_state: &Option<SubsystemState>,
) -> SubsystemState {
    if let Some(state) = override_state {
        return state.clone();
    }
    if ctx.callgraph_store_rx().lock().is_some() {
        return SubsystemState::Pending("building".to_string());
    }
    if ctx
        .callgraph_store()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
    {
        return SubsystemState::Ready;
    }
    // No store, no in-flight build: the cold build finished without producing a
    // store (failure already drained) — report ready so warmup doesn't hang.
    SubsystemState::Ready
}

fn drain_search_index_events(ctx: &AppContext) {
    let latest = {
        let rx_ref = ctx
            .search_index_rx()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut latest = None;
        while let Ok(index) = rx.try_recv() {
            latest = Some(index);
        }
        latest
    };

    if let Some(index) = latest {
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
    }
}

fn drain_semantic_index_events(ctx: &AppContext) {
    let events = {
        let rx_ref = ctx.semantic_index_rx().lock();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    };

    if events.is_empty() {
        return;
    }

    let mut keep_receiver = true;
    for event in events {
        match event {
            SemanticIndexEvent::Progress {
                stage,
                files,
                entries_done,
                entries_total,
            } => {
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::Building {
                        stage,
                        files,
                        entries_done,
                        entries_total,
                    };
            }
            SemanticIndexEvent::ColdSeedGateCleared => {
                ctx.resume_deferred_work_after_semantic_cold_seed_gate_cleared();
            }
            SemanticIndexEvent::Ready(index) => {
                *ctx.semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::ready();
                keep_receiver = false;
                ctx.clear_semantic_cold_seed_gate_and_resume_deferred_work();
            }
            SemanticIndexEvent::Failed(error) => {
                *ctx.semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::Failed(error);
                keep_receiver = false;
                ctx.clear_semantic_cold_seed_gate_and_resume_deferred_work();
            }
        }
    }

    if !keep_receiver {
        *ctx.semantic_index_rx().lock() = None;
    }
}

fn format_response_error(command: &str, response: Response) -> String {
    let code = response
        .data
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("error");
    let message = response
        .data
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown error");
    format!("aft warmup {command} failed ({code}): {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn default_warms_all_areas() {
        let parsed = WarmupArgs::parse(args(&["--root", "/tmp/x"])).unwrap();
        assert_eq!(parsed.areas, WarmupAreas::all());
        assert!(parsed.areas.search && parsed.areas.semantic && parsed.areas.callgraph);
    }

    // P1 regression: after the configure flat-read deletion, warmup must carry
    // search_index/semantic_search in a `config` tier doc (not flat params), or
    // handle_configure resolves them as disabled and warmup no-ops.
    #[test]
    fn warmup_configure_params_enable_requested_systems_via_tier_doc() {
        let root = std::path::Path::new("/tmp/proj");
        let storage = std::path::Path::new("/tmp/store");
        let areas = WarmupAreas {
            search: true,
            semantic: true,
            callgraph: true,
        };
        let params = build_warmup_configure_params(root, storage, areas, false);

        // Process-state stays flat.
        assert_eq!(params["storage_dir"], json!("/tmp/store"));
        // Core flags are NOT flat params (would be ignored by handle_configure).
        assert!(params.get("search_index").is_none());
        assert!(params.get("semantic_search").is_none());
        // They live in the synthetic user tier doc.
        let tiers = params["config"].as_array().expect("config tier array");
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0]["tier"], json!("user"));
        let doc: serde_json::Value =
            serde_json::from_str(tiers[0]["doc"].as_str().unwrap()).unwrap();
        assert_eq!(doc["search_index"], json!(true));
        assert_eq!(doc["semantic_search"], json!(true));

        // --only search → semantic disabled in the doc.
        let search_only = build_warmup_configure_params(
            root,
            storage,
            WarmupAreas {
                search: true,
                semantic: false,
                callgraph: false,
            },
            true,
        );
        let doc2: serde_json::Value =
            serde_json::from_str(search_only["config"][0]["doc"].as_str().unwrap()).unwrap();
        assert_eq!(doc2["search_index"], json!(true));
        assert_eq!(doc2["semantic_search"], json!(false));
        // force → internal bypass flag stays flat.
        assert_eq!(search_only["_bypass_size_limits"], json!(true));
    }

    #[test]
    fn only_single_area() {
        let parsed = WarmupArgs::parse(args(&["--root", "/tmp/x", "--only", "callgraph"])).unwrap();
        assert!(!parsed.areas.search);
        assert!(!parsed.areas.semantic);
        assert!(parsed.areas.callgraph);
    }

    #[test]
    fn only_multiple_areas_comma_separated() {
        let parsed =
            WarmupArgs::parse(args(&["--root", "/tmp/x", "--only", "search,semantic"])).unwrap();
        assert!(parsed.areas.search);
        assert!(parsed.areas.semantic);
        assert!(!parsed.areas.callgraph);
    }

    #[test]
    fn only_tolerates_whitespace_and_empty_segments() {
        let parsed = WarmupArgs::parse(args(&[
            "--root",
            "/tmp/x",
            "--only",
            " search , , callgraph ",
        ]))
        .unwrap();
        assert!(parsed.areas.search);
        assert!(!parsed.areas.semantic);
        assert!(parsed.areas.callgraph);
    }

    #[test]
    fn only_rejects_unknown_area() {
        let err = WarmupArgs::parse(args(&["--root", "/tmp/x", "--only", "lsp"])).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("unknown area 'lsp'"));
    }

    #[test]
    fn only_semantic_warmup_config_disables_callgraph_and_search() {
        let areas = WarmupAreas::parse_only("semantic").expect("parse --only semantic");
        assert_eq!(
            areas,
            WarmupAreas {
                search: false,
                semantic: true,
                callgraph: false,
            }
        );

        let root = std::path::Path::new("/tmp/aft-warmup-root");
        let storage = std::path::Path::new("/tmp/aft-warmup-storage");
        let params = build_warmup_configure_params(root, storage, areas, false);
        let doc = params["config"][0]["doc"]
            .as_str()
            .expect("warmup config doc should be a JSON string");
        let config_doc: serde_json::Value = serde_json::from_str(doc).expect("config doc parses");

        assert_eq!(config_doc["semantic_search"], serde_json::json!(true));
        assert_eq!(config_doc["search_index"], serde_json::json!(false));
        assert_eq!(config_doc["callgraph_store"], serde_json::json!(false));
    }

    #[test]
    fn only_rejects_empty_list() {
        let err = WarmupArgs::parse(args(&["--root", "/tmp/x", "--only", " , "])).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn only_requires_a_value() {
        let err = WarmupArgs::parse(args(&["--root", "/tmp/x", "--only"])).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("--only requires a value"));
    }

    #[test]
    fn try_set_ort_dylib_path_finds_cached_library() {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        std::env::remove_var("ORT_DYLIB_PATH");

        let temp = tempfile::tempdir().unwrap();
        let lib_name = if cfg!(target_os = "macos") {
            "libonnxruntime.dylib"
        } else if cfg!(target_os = "windows") {
            "onnxruntime.dll"
        } else {
            "libonnxruntime.so"
        };
        let ort_dir = temp.path().join("onnxruntime").join("1.24.4");
        std::fs::create_dir_all(&ort_dir).unwrap();
        let lib_path = ort_dir.join(lib_name);
        std::fs::write(&lib_path, b"fake").unwrap();

        try_set_ort_dylib_path(temp.path());

        assert_eq!(
            std::env::var_os("ORT_DYLIB_PATH"),
            Some(lib_path.as_os_str().to_owned())
        );
        std::env::remove_var("ORT_DYLIB_PATH");
    }
}
