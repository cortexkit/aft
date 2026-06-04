use aft::config::Config;
use aft::context::{AppContext, SemanticIndexEvent, SemanticIndexStatus};
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

    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
    configure(&ctx, &root, &storage_dir, args.force)?;
    wait_until_ready(&ctx, args.timeout_ms, args.quiet)
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

#[derive(Debug)]
struct WarmupArgs {
    root: Option<PathBuf>,
    timeout_ms: u64,
    quiet: bool,
    help: bool,
    /// Bypass the file-count caps (callgraph `max_callgraph_files`, semantic
    /// `semantic.max_files`) so a very large repo is fully indexed. Intended
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
    println!("aft warmup --root <absolute-path> [--timeout <ms>] [--quiet] [--force]");
    println!(
        "  --force  bypass file-count caps (callgraph + semantic) to fully index a large repo"
    );
}

fn warmup_storage_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("AFT_STORAGE_DIR") {
        return PathBuf::from(value);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".cache").join("aft")
}

fn configure(
    ctx: &AppContext,
    root: &std::path::Path,
    storage_dir: &std::path::Path,
    force: bool,
) -> Result<(), WarmupError> {
    let mut params = json!({
        "project_root": root.display().to_string(),
        "harness": "opencode",
        "search_index": true,
        "semantic_search": true,
        "storage_dir": storage_dir.display().to_string(),
    });
    if force {
        // Lift the file-count caps (callgraph `max_callgraph_files`, semantic
        // `semantic.max_files`, and the hardcoded search-index file limit) so a
        // very large repo is fully indexed for measurement. configure honors
        // this internal flag by raising the effective caps and skipping the
        // size-based auto-disable.
        params["_bypass_size_limits"] = json!(true);
    }
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

fn wait_until_ready(ctx: &AppContext, timeout_ms: u64, quiet: bool) -> Result<(), WarmupError> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut last_labels = WarmupLabels::default();
    loop {
        drain_search_index_events(ctx);
        drain_semantic_index_events(ctx);

        let snapshot = WarmupSnapshot::from_context(ctx);
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
}

impl WarmupSnapshot {
    fn from_context(ctx: &AppContext) -> Self {
        let search_index = search_index_state(ctx);
        let semantic_index = semantic_index_state(ctx);
        let symbol_cache = symbol_cache_state(&search_index);
        Self {
            search_index,
            semantic_index,
            symbol_cache,
        }
    }

    fn is_terminal(&self) -> bool {
        self.search_index.is_terminal()
            && self.semantic_index.is_terminal()
            && self.symbol_cache.is_terminal()
    }

    fn labels(&self) -> WarmupLabels {
        WarmupLabels {
            search_index: self.search_index.label(),
            semantic_index: self.semantic_index.label(),
            symbol_cache: self.symbol_cache.label(),
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
    if ctx
        .search_index()
        .borrow()
        .as_ref()
        .is_some_and(|index| index.ready)
    {
        return SubsystemState::Ready;
    }
    if ctx.search_index_rx().borrow().is_some() {
        SubsystemState::Pending("building".to_string())
    } else {
        SubsystemState::Pending("loading".to_string())
    }
}

fn semantic_index_state(ctx: &AppContext) -> SubsystemState {
    if !ctx.config().semantic_search {
        return SubsystemState::Disabled;
    }
    match ctx.semantic_index_status().borrow().clone() {
        SemanticIndexStatus::Disabled => SubsystemState::Disabled,
        SemanticIndexStatus::Ready { .. } => SubsystemState::Ready,
        SemanticIndexStatus::Failed(error) => SubsystemState::Failed(error),
        SemanticIndexStatus::Building {
            stage,
            files,
            entries_done,
            entries_total,
            ..
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

fn drain_search_index_events(ctx: &AppContext) {
    let latest = {
        let rx_ref = ctx.search_index_rx().borrow();
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
        *ctx.search_index().borrow_mut() = Some(index);
    }
}

fn drain_semantic_index_events(ctx: &AppContext) {
    let events = {
        let rx_ref = ctx.semantic_index_rx().borrow();
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
                stats,
            } => {
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
                    stage,
                    files,
                    entries_done,
                    entries_total,
                    stats,
                };
            }
            SemanticIndexEvent::Ready(index) => {
                *ctx.semantic_index().borrow_mut() = Some(index);
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
                keep_receiver = false;
            }
            SemanticIndexEvent::Failed(error) => {
                *ctx.semantic_index().borrow_mut() = None;
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(error);
                keep_receiver = false;
            }
        }
    }

    if !keep_receiver {
        *ctx.semantic_index_rx().borrow_mut() = None;
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
