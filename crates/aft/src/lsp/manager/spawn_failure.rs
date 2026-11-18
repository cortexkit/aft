use super::*;

pub(super) fn log_spawn_failure(
    def: &ServerDef,
    key: &ServerKey,
    error: &LspError,
    durability: FailureDurability,
    now: Instant,
) {
    let capacity = match error {
        LspError::Io(io) => io.get_ref().and_then(|cause| {
            cause.downcast_ref::<crate::lsp::client::RustAnalyzerCapacityExhausted>()
        }),
        _ => None,
    };
    if let Some(capacity) = capacity {
        // Admission is daemon-wide, so separate managers for the same root
        // must share the log limit. Expire quiet roots instead of retaining
        // every checkout the daemon has ever seen.
        static LAST_LOG: std::sync::OnceLock<parking_lot::Mutex<HashMap<PathBuf, Instant>>> =
            std::sync::OnceLock::new();
        let mut last_log = LAST_LOG.get_or_init(Default::default).lock();
        last_log.retain(|_, at| now.saturating_duration_since(*at) < Duration::from_secs(3600));
        if last_log.contains_key(&key.root) {
            return;
        }
        last_log.insert(key.root.clone(), now);
        drop(last_log);
        slog_info!(
            "rust-analyzer admission deferred root={} cap={} current={}; retry after another worker analyzer stops",
            key.root.display(),
            capacity.cap,
            capacity.current
        );
    } else if durability == FailureDurability::Environment
        || matches!(error, LspError::TypeScriptSdkUnavailable(_))
        || matches!(error, LspError::ServerNotReady(reason) if reason.starts_with(NOT_IN_TYPESCRIPT_PROJECT))
        || (durability == FailureDurability::Permanent
            && typescript_sdk_error(&error.to_string(), ""))
    {
        slog_warn!(
            "language server not started (project environment): {}: {}",
            def.name,
            error
        );
    } else {
        slog_error!("failed to spawn {}: {}", def.name, error);
    }
}

pub(super) fn typescript_sdk_error(reason: &str, stderr_tail: &str) -> bool {
    use crate::lsp::environmental::{TS_LS5_NO_INSTALLATION, TS_LS6_NO_TSSERVER};

    [reason, stderr_tail]
        .iter()
        .any(|text| text.contains(TS_LS5_NO_INSTALLATION) || text.contains(TS_LS6_NO_TSSERVER))
}
