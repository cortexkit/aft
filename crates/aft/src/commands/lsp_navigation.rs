use std::path::Path;
use std::sync::{mpsc, Arc};
#[cfg(test)]
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use crate::response_finalize::{DispatchOutcome, PendingResponse};

const DEFERRED_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(test)]
struct DeferredNavigationGate {
    started: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}

#[cfg(test)]
static DEFERRED_NAVIGATION_GATE: LazyLock<Mutex<Option<DeferredNavigationGate>>> =
    LazyLock::new(|| Mutex::new(None));
#[cfg(test)]
static DEFERRED_NAVIGATION_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
#[cfg(test)]
static DEFERRED_NAVIGATION_WORKERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

type NavigationHandler = fn(&RawRequest, &AppContext) -> Response;

#[cfg(test)]
pub(crate) fn deferred_navigation_test_lock() -> std::sync::MutexGuard<'static, ()> {
    DEFERRED_NAVIGATION_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) fn install_deferred_navigation_gate_for_test(
) -> (mpsc::Receiver<()>, mpsc::SyncSender<()>) {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    *DEFERRED_NAVIGATION_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(DeferredNavigationGate {
        started: started_tx,
        release: release_rx,
    });
    (started_rx, release_tx)
}

#[cfg(test)]
static DEFERRED_NAVIGATION_SEEN_RESTRICT: Mutex<Option<bool>> = Mutex::new(None);

/// What `restrict_to_project_root` the last deferred navigation worker saw
/// once past its test gate.
#[cfg(test)]
pub(crate) fn take_deferred_navigation_seen_restrict_for_test() -> Option<bool> {
    DEFERRED_NAVIGATION_SEEN_RESTRICT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

#[cfg(test)]
fn record_deferred_navigation_config_for_test(ctx: &AppContext) {
    *DEFERRED_NAVIGATION_SEEN_RESTRICT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(ctx.config().restrict_to_project_root);
}

#[cfg(test)]
pub(crate) fn deferred_navigation_worker_count_for_test() -> usize {
    DEFERRED_NAVIGATION_WORKERS.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn is_lsp_navigation_command(command: &str) -> bool {
    matches!(
        command,
        "lsp_hover" | "lsp_goto_definition" | "lsp_find_references" | "lsp_prepare_rename"
    )
}

pub fn handle_lsp_navigation_deferred(req: &RawRequest, ctx: Arc<AppContext>) -> DispatchOutcome {
    handle_lsp_navigation_deferred_with_restriction(req, ctx, false)
}

pub(crate) fn handle_lsp_navigation_deferred_with_restriction(
    req: &RawRequest,
    ctx: Arc<AppContext>,
    force_restrict: bool,
) -> DispatchOutcome {
    let Some(handler) = navigation_handler(&req.command) else {
        return DispatchOutcome::Immediate(Response::error(
            &req.id,
            "unknown_command",
            format!("unknown LSP navigation command: {}", req.command),
        ));
    };

    if !navigation_requires_deferred_execution(req, &ctx) {
        return DispatchOutcome::Immediate(handler(req, &ctx));
    }

    defer_lsp_navigation(req, ctx, force_restrict, handler)
}

fn navigation_handler(command: &str) -> Option<NavigationHandler> {
    match command {
        "lsp_hover" => Some(crate::commands::lsp_hover::handle_lsp_hover),
        "lsp_goto_definition" => {
            Some(crate::commands::lsp_goto_definition::handle_lsp_goto_definition)
        }
        "lsp_find_references" => {
            Some(crate::commands::lsp_find_references::handle_lsp_find_references)
        }
        "lsp_prepare_rename" => {
            Some(crate::commands::lsp_prepare_rename::handle_lsp_prepare_rename)
        }
        _ => None,
    }
}

fn navigation_requires_deferred_execution(req: &RawRequest, ctx: &AppContext) -> bool {
    let Some(file) = req.params.get("file").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Ok(file_path) = ctx.validate_path(&req.id, Path::new(file)) else {
        return false;
    };
    let config = ctx.config();
    ctx.lsp()
        .navigation_requires_deferred_execution(&file_path, &config)
}

#[cfg(test)]
fn wait_at_deferred_navigation_gate_for_test() {
    let gate = DEFERRED_NAVIGATION_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(gate) = gate else {
        return;
    };
    let _ = gate.started.send(());
    loop {
        if crate::executor::current_job_cancelled() {
            return;
        }
        match gate.release.recv_timeout(Duration::from_millis(5)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(not(test))]
fn wait_at_deferred_navigation_gate_for_test() {}

#[cfg(test)]
struct DeferredNavigationWorkerGuard;

#[cfg(test)]
impl DeferredNavigationWorkerGuard {
    fn new() -> Self {
        DEFERRED_NAVIGATION_WORKERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

#[cfg(test)]
impl Drop for DeferredNavigationWorkerGuard {
    fn drop(&mut self) {
        DEFERRED_NAVIGATION_WORKERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn defer_lsp_navigation(
    req: &RawRequest,
    ctx: Arc<AppContext>,
    force_restrict: bool,
    handler: NavigationHandler,
) -> DispatchOutcome {
    let request = RawRequest {
        id: req.id.clone(),
        command: req.command.clone(),
        lsp_hints: req.lsp_hints.clone(),
        session_id: req.session_id.clone(),
        params: req.params.clone(),
    };
    let request_id = req.id.clone();
    let timeout_request_id = request_id.clone();
    let command = req.command.clone();
    let timeout_command = command.clone();
    let deadline = Instant::now() + DEFERRED_NAVIGATION_TIMEOUT;
    let cancellation = crate::executor::current_job_cancellation()
        .unwrap_or_else(crate::executor::JobCancellation::new);
    let worker_cancellation = cancellation.clone();
    let timeout_cancellation = cancellation.clone();
    let (tx, rx) = crate::response_finalize::pending_response_channel();
    // The request's admitted config, installed on the worker below.
    let admitted_config = ctx.config();

    // Cold initialization and its first query run after the scheduler job returns,
    // so an LSP handshake cannot serialize unrelated work on the same root.
    std::thread::spawn(move || {
        #[cfg(test)]
        let _worker = DeferredNavigationWorkerGuard::new();
        let _config_pin = ctx.pin_config_to(admitted_config);
        let _cancellation = crate::executor::install_job_cancellation(worker_cancellation);
        let _force_restrict = force_restrict.then(|| ctx.force_restrict_guard(&request.id));
        wait_at_deferred_navigation_gate_for_test();
        #[cfg(test)]
        record_deferred_navigation_config_for_test(&ctx);
        let response = if crate::executor::current_job_cancelled() {
            navigation_cancelled_response(&request.id, &request.command)
        } else {
            handler(&request, &ctx)
        };
        let _ = tx.send(response);
    });

    let mut settled = false;
    DispatchOutcome::Deferred(PendingResponse::from_receiver(
        request_id,
        req.session().to_string(),
        command,
        rx,
        move |_, completion| {
            if settled {
                return None;
            }
            match completion {
                Ok(response) => {
                    settled = true;
                    Some(response)
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    settled = true;
                    Some(Response::error(
                        &timeout_request_id,
                        "lsp_error",
                        format!("{timeout_command}: deferred request worker disconnected"),
                    ))
                }
                Err(mpsc::TryRecvError::Empty) if Instant::now() >= deadline => {
                    settled = true;
                    timeout_cancellation.request_cancel();
                    Some(Response::error(
                        &timeout_request_id,
                        "lsp_error",
                        format!(
                            "{timeout_command}: request failed: deferred request exceeded its overall deadline"
                        ),
                    ))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            }
        },
    ).with_cancellation(cancellation)
        .with_poll_interval(Duration::from_millis(100)))
}

fn navigation_cancelled_response(request_id: &str, command: &str) -> Response {
    Response::error(
        request_id,
        "lsp_error",
        format!("{command}: request cancelled before execution"),
    )
}
