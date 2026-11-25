//! Route-bind replies and safe logging of daemon control decode errors.
use super::*;

pub(super) fn route_bind_ack_frame(ver: u8, corr: u64, flags: Flags) -> Result<Frame, SubcError> {
    let body =
        serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).map_err(SubcError::Json)?;
    Frame::build_with_version(ver, FrameType::Response, flags, 0, 0, corr, body)
        .map_err(SubcError::FrameBuild)
}

/// Longest decode-error text a refusal carries back to the sender; serde can
/// quote a whole body in its message.
const DECODE_ERROR_TEXT_MAX: usize = 240;

/// A serde decode error, shortened for a refusal message and a log line.
pub(super) fn brief_decode_error(error: &serde_json::Error) -> String {
    let text = error.to_string();
    match text.char_indices().nth(DECODE_ERROR_TEXT_MAX) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text,
    }
}

/// Answers a channel-0 request whose body does not decode, and keeps the
/// module running.
///
/// One request AFT cannot read (for example a route bind with an unfamiliar
/// field in the daemon's session identity) must cost
/// only that request. Ending the frame loop instead would drop every route on
/// the connection, fleet-wide, until the daemon respawned the module. Only
/// transport failures may end the loop.
pub(super) async fn refuse_undecodable_control_request(
    tx: &WriterSender,
    frame: &Frame,
    error: &serde_json::Error,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let reason = brief_decode_error(error);
    log::warn!(
        "subc attach: refusing channel-0 request corr={} whose body does not decode ({} bytes): {reason}",
        frame.header.corr,
        frame.body.len()
    );
    let response = build_error_frame(
        frame.header.ver,
        0,
        0,
        frame.header.corr,
        frame.header.flags,
        UNDECODABLE_CONTROL_REQUEST,
        &format!("control request does not decode: {reason}"),
    )?;
    send_reliable_writer_frame(tx, metrics, response, "undecodable control request error").await
}

/// Error code for a channel-0 request AFT could not decode.
pub(crate) const UNDECODABLE_CONTROL_REQUEST: &str = "undecodable_control_request";

pub(super) async fn send_route_bind_error(
    tx: &WriterSender,
    frame: &Frame,
    code: &str,
    message: &str,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    send_route_bind_error_parts(
        tx,
        frame.header.ver,
        frame.header.corr,
        frame.header.flags,
        code,
        message,
        metrics,
    )
    .await
}

/// [`send_route_bind_error`] for a refusal that carries a machine-readable
/// `detail`, such as the role versions AFT does serve.
pub(super) async fn send_route_bind_error_body(
    tx: &WriterSender,
    frame: &Frame,
    refusal: &ErrorBody,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    send_route_bind_error_body_parts(
        tx,
        frame.header.ver,
        frame.header.corr,
        frame.header.flags,
        refusal,
        metrics,
    )
    .await
}

pub(super) async fn send_route_bind_error_body_parts(
    tx: &WriterSender,
    ver: u8,
    corr: u64,
    flags: Flags,
    refusal: &ErrorBody,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let body = serde_json::to_vec(refusal).map_err(SubcError::Json)?;
    let response = Frame::build_with_version(ver, FrameType::Error, flags, 0, 0, corr, body)
        .map_err(SubcError::FrameBuild)?;
    send_reliable_writer_frame(tx, metrics, response, "RouteBind error").await?;
    log_route_bind_rejection(&refusal.code, &refusal.message);
    Ok(())
}

pub(super) async fn send_route_bind_error_parts(
    tx: &WriterSender,
    ver: u8,
    corr: u64,
    flags: Flags,
    code: &str,
    message: &str,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let response = build_error_frame(ver, 0, 0, corr, flags, code, message)?;
    send_reliable_writer_frame(tx, metrics, response, "RouteBind error").await?;
    log_route_bind_rejection(code, message);
    Ok(())
}

/// Per-message rate limit for the bind-rejection warn line. A caller that
/// re-attaches a dead root forever turns this line into the entire readable
/// tail of the SHARED daemon log (measured 2026-08-09: 2.08M copies, ~40/sec,
/// 936MB log — other modules' incident lines pushed out of the tail).
/// The line itself stays byte-identical so external counters keep matching;
/// repeats inside the window are summarized with a suppressed count on the
/// next emission so repeated refusals remain countable without flooding logs.
fn log_route_bind_rejection(code: &str, message: &str) {
    const WINDOW: Duration = Duration::from_secs(60);
    static SUPPRESSED: OnceLock<StdMutex<HashMap<String, (Instant, u64)>>> = OnceLock::new();
    let map = SUPPRESSED.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut map = match map.try_lock() {
        Ok(map) => map,
        // Contended: log unsuppressed rather than blocking or dropping.
        Err(_) => {
            log::warn!("subc attach: route bind rejected ({code}): {message}");
            return;
        }
    };
    let now = Instant::now();
    // Bound the map: dead roots churn, and an unbounded suppression map is
    // its own leak. Sweep expired entries once it grows past a fleet-sized
    // number of distinct rejection messages.
    if map.len() > 512 {
        map.retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
    }
    match map.get_mut(message) {
        Some((window_start, suppressed)) if now.duration_since(*window_start) < WINDOW => {
            *suppressed += 1;
        }
        Some((window_start, suppressed)) => {
            if *suppressed > 0 {
                log::warn!(
                    "subc attach: route bind rejected ({code}): {message} (repeated {}x in last 60s)",
                    *suppressed
                );
            } else {
                log::warn!("subc attach: route bind rejected ({code}): {message}");
            }
            *window_start = now;
            *suppressed = 0;
        }
        None => {
            log::warn!("subc attach: route bind rejected ({code}): {message}");
            map.insert(message.to_string(), (now, 0));
        }
    }
}
