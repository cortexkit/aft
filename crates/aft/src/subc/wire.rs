//! Frame encoding and writer-queue helpers used by the subc transport edge.

#[cfg(test)]
use serde::ser::{SerializeMap, SerializeStruct};
#[cfg(test)]
use serde::{Serialize, Serializer};

use super::{
    control_flags, fmt, mpsc, Arc, AtomicUsize, BindTrust, DispatchPathMetrics, ErrorBody, Flags,
    Frame, FrameType, Ordering, PathBuf, Response, RouteChannel, ToolCallResult, Value,
    CONTROL_SEND_TIMEOUT, RELIABLE_WRITER_RETRY_INITIAL_BACKOFF, RELIABLE_WRITER_RETRY_MAX_BACKOFF,
};
use crate::run_tool_call::{PhaseTrace, ToolCallEgressTiming, ToolCallPhaseDurations};
use std::borrow::Cow;
use subc_protocol::{FrameBuildError, MAX_FRAME_BODY_LEN};

pub(super) type WriterSender = mpsc::Sender<WriterFrame>;

pub(super) struct ToolResponseWriteTrace {
    phase_trace: PhaseTrace,
    name: String,
    root: PathBuf,
    session: String,
    channel: u16,
    corr: u64,
    enqueued_at: Option<std::time::Instant>,
    queue_depth: usize,
    writer_active_at_enqueue: bool,
    writer_queue_was_full: bool,
    reserve_timeouts: u32,
}

impl ToolResponseWriteTrace {
    pub(super) fn new(
        phase_trace: PhaseTrace,
        name: String,
        root: PathBuf,
        session: String,
        channel: u16,
        corr: u64,
    ) -> Self {
        Self {
            phase_trace,
            name,
            root,
            session,
            channel,
            corr,
            enqueued_at: None,
            queue_depth: 0,
            writer_active_at_enqueue: false,
            writer_queue_was_full: false,
            reserve_timeouts: 0,
        }
    }

    fn mark_writer_queue_full(&mut self) {
        self.writer_queue_was_full = true;
    }

    fn mark_reserve_timeout(&mut self) {
        self.reserve_timeouts = self.reserve_timeouts.saturating_add(1);
    }

    fn mark_enqueued(&mut self, queue_depth: usize, writer_active: bool) {
        self.enqueued_at = Some(std::time::Instant::now());
        self.queue_depth = queue_depth;
        self.writer_active_at_enqueue = writer_active;
    }

    pub(super) fn finish(
        self,
        dequeued: std::time::Instant,
        write_started: std::time::Instant,
        write_finished: std::time::Instant,
        frame_bytes: usize,
    ) -> Option<CompletedToolResponseTrace> {
        let phases = self.phase_trace.finish(ToolCallEgressTiming {
            enqueued: self.enqueued_at?,
            dequeued,
            write_started,
            write_finished,
            frame_bytes,
            queue_depth: self.queue_depth,
            writer_active_at_enqueue: self.writer_active_at_enqueue,
            writer_queue_was_full: self.writer_queue_was_full,
            reserve_timeouts: self.reserve_timeouts,
        })?;
        Some(CompletedToolResponseTrace {
            name: self.name,
            root: self.root,
            session: self.session,
            channel: self.channel,
            corr: self.corr,
            phases,
        })
    }
}

pub(super) struct CompletedToolResponseTrace {
    pub(super) name: String,
    pub(super) root: PathBuf,
    pub(super) session: String,
    pub(super) channel: u16,
    pub(super) corr: u64,
    pub(super) phases: ToolCallPhaseDurations,
}

enum WriterFrameBody {
    Owned,
    SharedPush(Arc<Vec<u8>>),
}

pub(super) struct WriterFrame {
    /// The route-specific header. Shared Push bodies remain outside this frame.
    pub(super) frame: Frame,
    body: WriterFrameBody,
    pub(super) tool_response_trace: Option<ToolResponseWriteTrace>,
}

impl std::ops::Deref for WriterFrame {
    type Target = Frame;

    fn deref(&self) -> &Self::Target {
        &self.frame
    }
}

impl WriterFrame {
    pub(super) fn plain(frame: Frame) -> Self {
        Self {
            frame,
            body: WriterFrameBody::Owned,
            tool_response_trace: None,
        }
    }

    pub(super) fn shared_push(frame: Frame, body: Arc<Vec<u8>>) -> Self {
        debug_assert_eq!(frame.header.len as usize, body.len());
        Self {
            frame,
            body: WriterFrameBody::SharedPush(body),
            tool_response_trace: None,
        }
    }

    fn traced_tool_response(frame: Frame, trace: ToolResponseWriteTrace) -> Self {
        Self {
            frame,
            body: WriterFrameBody::Owned,
            tool_response_trace: Some(trace),
        }
    }

    pub(super) fn frame(&self) -> &Frame {
        &self.frame
    }

    pub(super) fn body(&self) -> &[u8] {
        match &self.body {
            WriterFrameBody::Owned => &self.frame.body,
            WriterFrameBody::SharedPush(body) => body,
        }
    }

    #[cfg(test)]
    pub(super) fn shared_push_body_strong_count(&self) -> Option<usize> {
        match &self.body {
            WriterFrameBody::Owned => None,
            WriterFrameBody::SharedPush(body) => Some(Arc::strong_count(body)),
        }
    }

    fn mark_writer_queue_full(&mut self) {
        if let Some(trace) = self.tool_response_trace.as_mut() {
            trace.mark_writer_queue_full();
        }
    }

    fn mark_reserve_timeout(&mut self) {
        if let Some(trace) = self.tool_response_trace.as_mut() {
            trace.mark_reserve_timeout();
        }
    }

    fn mark_enqueued(&mut self, queue_depth: usize, writer_active: bool) {
        if let Some(trace) = self.tool_response_trace.as_mut() {
            trace.mark_enqueued(queue_depth, writer_active);
        }
    }
}

pub(super) enum WriterEnqueueOutcome {
    Enqueued,
    Full(WriterFrame),
    Closed,
}

impl WriterEnqueueOutcome {
    #[cfg(test)]
    pub(super) fn is_enqueued(&self) -> bool {
        matches!(self, Self::Enqueued)
    }
}

pub(super) fn decrement_counted_channel(counter: &AtomicUsize) {
    let previous = counter.fetch_sub(1, Ordering::Relaxed);
    debug_assert!(previous > 0, "counted channel depth underflow");
}

pub(super) async fn send_counted_channel<T>(
    tx: &mpsc::Sender<T>,
    counter: &AtomicUsize,
    item: T,
) -> Result<(), mpsc::error::SendError<T>> {
    counter.fetch_add(1, Ordering::Relaxed);
    match tx.send(item).await {
        Ok(()) => Ok(()),
        Err(error) => {
            decrement_counted_channel(counter);
            Err(error)
        }
    }
}

fn enqueue_writer_item(
    permit: mpsc::Permit<'_, WriterFrame>,
    metrics: &DispatchPathMetrics,
    mut item: WriterFrame,
) {
    let queue_depth = metrics.writer_queued.fetch_add(1, Ordering::Relaxed) + 1;
    item.mark_enqueued(queue_depth, metrics.writer_active.load(Ordering::Relaxed));
    permit.send(item);
}

fn try_enqueue_writer_item(
    tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    mut item: WriterFrame,
) -> WriterEnqueueOutcome {
    match tx.try_reserve() {
        Ok(permit) => {
            enqueue_writer_item(permit, metrics, item);
            WriterEnqueueOutcome::Enqueued
        }
        Err(mpsc::error::TrySendError::Full(())) => {
            metrics
                .writer_saturation_count
                .fetch_add(1, Ordering::Relaxed);
            item.mark_writer_queue_full();
            WriterEnqueueOutcome::Full(item)
        }
        Err(mpsc::error::TrySendError::Closed(())) => {
            drop(item);
            WriterEnqueueOutcome::Closed
        }
    }
}

pub(super) fn try_enqueue_writer_frame(
    tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    frame: Frame,
) -> WriterEnqueueOutcome {
    try_enqueue_writer_item(tx, metrics, WriterFrame::plain(frame))
}

pub(super) fn try_enqueue_shared_push_frame(
    tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    frame: Frame,
    body: Arc<Vec<u8>>,
) -> WriterEnqueueOutcome {
    try_enqueue_writer_item(tx, metrics, WriterFrame::shared_push(frame, body))
}

async fn send_reliable_writer_item(
    tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    mut item: WriterFrame,
    context: &'static str,
) -> Result<(), SubcError> {
    let mut warned = false;
    let mut backoff = RELIABLE_WRITER_RETRY_INITIAL_BACKOFF;

    loop {
        match try_enqueue_writer_item(tx, metrics, item) {
            WriterEnqueueOutcome::Enqueued => return Ok(()),
            WriterEnqueueOutcome::Closed => return Err(SubcError::WriterClosed),
            WriterEnqueueOutcome::Full(returned_item) => {
                item = returned_item;
            }
        }

        match tokio::time::timeout(CONTROL_SEND_TIMEOUT, tx.reserve()).await {
            Ok(Ok(permit)) => {
                enqueue_writer_item(permit, metrics, item);
                return Ok(());
            }
            Ok(Err(_)) => return Err(SubcError::WriterClosed),
            Err(_) => {
                metrics
                    .writer_saturation_count
                    .fetch_add(1, Ordering::Relaxed);
                item.mark_reserve_timeout();
                if !warned {
                    log::warn!(
                        "subc attach: writer queue stayed full while sending {context}; retrying reliable frame"
                    );
                    warned = true;
                }
                tokio::time::sleep(backoff).await;
                backoff =
                    std::cmp::min(backoff.saturating_mul(2), RELIABLE_WRITER_RETRY_MAX_BACKOFF);
            }
        }
    }
}

pub(super) async fn send_reliable_writer_frame(
    tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    frame: Frame,
    context: &'static str,
) -> Result<(), SubcError> {
    send_reliable_writer_item(tx, metrics, WriterFrame::plain(frame), context).await
}

pub(super) async fn send_traced_tool_response_frame(
    tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    frame: Frame,
    trace: ToolResponseWriteTrace,
) -> Result<(), SubcError> {
    send_reliable_writer_item(
        tx,
        metrics,
        WriterFrame::traced_tool_response(frame, trace),
        "tool response",
    )
    .await
}

pub(super) async fn send_frame(
    tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    frame: Frame,
) -> Result<(), SubcError> {
    match try_enqueue_writer_item(tx, metrics, WriterFrame::plain(frame)) {
        WriterEnqueueOutcome::Enqueued => Ok(()),
        WriterEnqueueOutcome::Closed => Err(SubcError::WriterClosed),
        WriterEnqueueOutcome::Full(item) => {
            match tokio::time::timeout(CONTROL_SEND_TIMEOUT, tx.reserve()).await {
                Ok(Ok(permit)) => {
                    enqueue_writer_item(permit, metrics, item);
                    Ok(())
                }
                Ok(Err(_)) => Err(SubcError::WriterClosed),
                Err(_) => {
                    metrics
                        .writer_saturation_count
                        .fetch_add(1, Ordering::Relaxed);
                    Err(SubcError::WriterBackpressureTimeout)
                }
            }
        }
    }
}

/// Borrowed flat response matching the standalone NDJSON shape without cloning
/// the response id or any structured data values.
#[cfg(test)]
struct FlatToolResponse<'a> {
    response: &'a crate::protocol::Response,
    text: &'a str,
}

#[cfg(test)]
impl Serialize for FlatToolResponse<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let data = self.response.data.as_object();
        let has_text = data.is_some_and(|data| data.contains_key("text"));
        let field_count =
            2 + data.map_or(0, |data| {
                data.len()
                    - usize::from(data.contains_key("id"))
                    - usize::from(data.contains_key("success"))
            }) + usize::from(!has_text);
        let mut map = serializer.serialize_map(Some(field_count))?;
        match data.and_then(|data| data.get("id")) {
            Some(value) => map.serialize_entry("id", value)?,
            None => map.serialize_entry("id", &self.response.id)?,
        }
        match data.and_then(|data| data.get("success")) {
            Some(value) => map.serialize_entry("success", value)?,
            None => map.serialize_entry("success", &self.response.success)?,
        }
        if let Some(data) = data {
            for (key, value) in data {
                match key.as_str() {
                    "id" | "success" => {}
                    "text" => map.serialize_entry(key, self.text)?,
                    _ => map.serialize_entry(key, value)?,
                }
            }
        }
        if !has_text {
            map.serialize_entry("text", self.text)?;
        }
        map.end()
    }
}

#[cfg(test)]
struct ToolResponseEnvelope<'a> {
    result: &'a ToolCallResult,
    /// First-party binds get the full flat response in `structuredContent`
    /// for the plugin re-lift; untrusted (MCP) binds get text-only.
    include_structured: bool,
}

// A trusted envelope carries the rendered text twice — once as the outer MCP
// `content`, once inside `structuredContent` — and for reads the raw `content`
// data field rides along a third time, so a read body crosses the connection
// roughly 3x. This is deliberate, not an oversight: the bridge re-lifts
// `structuredContent` to reconstruct the flat response. The bridge's
// `reliftReply` now tolerates omission of `structuredContent.text`, but AFT
// still emits it until every supported plugin version includes that fallback.
// Only then can the duplicate be dropped behind the plugin version floor.
//
// Measured on a live daemon: the largest real frames were ~200 KB, with zero
// egress-write time, writer queue depth 1, never full, and no reserve
// timeouts — the amplification costs nothing observable. Revisit if
// If `egress_write` becomes nonzero, the writer queue backs up, or typical
// frames grow well past a few hundred KB, the duplicate content is costly
// enough to justify dropping `structuredContent.text` after the plugin floor
// makes that omission compatible.

#[cfg(test)]
impl Serialize for ToolResponseEnvelope<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let fields = if self.include_structured { 3 } else { 2 };
        let mut envelope = serializer.serialize_struct("ToolResponseEnvelope", fields)?;
        envelope.serialize_field(
            "content",
            &[TextContent {
                kind: "text",
                text: &self.result.text,
            }],
        )?;
        envelope.serialize_field("isError", &!self.result.response.success)?;
        if self.include_structured {
            envelope.serialize_field(
                "structuredContent",
                &FlatToolResponse {
                    response: &self.result.response,
                    text: &self.result.text,
                },
            )?;
        }
        envelope.end()
    }
}

#[cfg(test)]
#[derive(Serialize)]
struct TextContent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

const TRANSPORT_MIB: usize = 1024 * 1024;
const TOOL_RESPONSE_ENVELOPE_MARGIN_DIVISOR: usize = 8;
const RESPONSE_TOO_LARGE_CODE: &str = "response_too_large";
const TRANSPORT_TRUNCATION_REASON: &str = "transport_frame_limit";

fn transport_limit_mib(bytes: usize) -> usize {
    bytes.div_ceil(TRANSPORT_MIB).max(1)
}

fn tool_response_text_limit(max_body_len: usize, include_structured: bool) -> usize {
    // Trusted binds carry rendered text in both MCP content and structuredContent.
    // Keep one eighth free for the envelope and sidecars; anything larger there
    // is handled by the correlated fallback instead of silently dropping the call.
    let margin = (max_body_len / TOOL_RESPONSE_ENVELOPE_MARGIN_DIVISOR)
        .max(1_024)
        .min(max_body_len / 2);
    let rendered_copies = if include_structured { 2 } else { 1 };
    max_body_len.saturating_sub(margin) / rendered_copies
}

fn truncate_rendered_text(text: &str, limit: usize) -> Cow<'_, str> {
    if text.len() <= limit {
        return Cow::Borrowed(text);
    }

    let notice = format!(
        "[response truncated at {} MiB: full output exceeds the transport frame limit; use offset/limit or write to a file]",
        transport_limit_mib(limit)
    );
    let suffix = format!("\n{notice}");
    let mut prefix_end = text.len().min(limit.saturating_sub(suffix.len()));
    while !text.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }

    let mut truncated = String::with_capacity(prefix_end.saturating_add(suffix.len()));
    truncated.push_str(&text[..prefix_end]);
    truncated.push_str(&suffix);
    Cow::Owned(truncated)
}

fn serialize_tool_response_body(
    result: &ToolCallResult,
    include_structured: bool,
) -> Result<Vec<u8>, serde_json::Error> {
    serialize_tool_response_body_with_text(result, include_structured, &result.text, false)
}

fn serialize_tool_response_body_with_text(
    result: &ToolCallResult,
    include_structured: bool,
    rendered_text: &str,
    transport_truncated: bool,
) -> Result<Vec<u8>, serde_json::Error> {
    let data_capacity = result
        .response
        .data
        .as_object()
        .and_then(|data| {
            ["content", "output", "preview_diff"]
                .into_iter()
                .find_map(|key| data.get(key).and_then(Value::as_str))
                .map(|data_text| {
                    if transport_truncated && data_text == result.text.as_str() {
                        rendered_text.len()
                    } else {
                        data_text.len()
                    }
                })
        })
        .unwrap_or(0);
    let capacity = rendered_text
        .len()
        .saturating_add(include_structured.then_some(data_capacity).unwrap_or(0))
        .saturating_add(
            include_structured
                .then_some(rendered_text.len())
                .unwrap_or(0),
        )
        .saturating_add(512);
    let mut body = Vec::with_capacity(capacity);

    body.extend_from_slice(b"{\"content\":[{\"type\":\"text\",\"text\":");
    let encoded_text_start = body.len();
    serde_json::to_writer(&mut body, rendered_text)?;
    let encoded_text_end = body.len();
    body.extend_from_slice(b"}],\"isError\":");
    body.extend_from_slice(if result.response.success {
        b"false"
    } else {
        b"true"
    });

    if include_structured {
        body.extend_from_slice(b",\"structuredContent\":{\"id\":");
        let data = result.response.data.as_object();
        match data.and_then(|data| data.get("id")) {
            Some(value) => serde_json::to_writer(&mut body, value)?,
            None => serde_json::to_writer(&mut body, &result.response.id)?,
        }
        body.extend_from_slice(b",\"success\":");
        match data.and_then(|data| data.get("success")) {
            Some(value) => serde_json::to_writer(&mut body, value)?,
            None => body.extend_from_slice(if result.response.success {
                b"true"
            } else {
                b"false"
            }),
        }

        let mut has_text = false;
        let mut has_complete = false;
        let mut has_truncated = false;
        let mut has_truncation_reason = false;
        if let Some(data) = data {
            for (key, value) in data {
                match key.as_str() {
                    "id" | "success" => continue,
                    "text" => has_text = true,
                    "complete" => has_complete = true,
                    "truncated" => has_truncated = true,
                    "truncation_reason" => has_truncation_reason = true,
                    _ => {}
                }
                body.push(b',');
                serde_json::to_writer(&mut body, key)?;
                body.push(b':');
                match key.as_str() {
                    "text" => body.extend_from_within(encoded_text_start..encoded_text_end),
                    "complete" if transport_truncated => body.extend_from_slice(b"false"),
                    "truncated" if transport_truncated => body.extend_from_slice(b"true"),
                    "truncation_reason" if transport_truncated => {
                        serde_json::to_writer(&mut body, TRANSPORT_TRUNCATION_REASON)?;
                    }
                    _ if value.as_str() == Some(result.text.as_str()) => {
                        body.extend_from_within(encoded_text_start..encoded_text_end);
                    }
                    _ => serde_json::to_writer(&mut body, value)?,
                }
            }
        }
        if !has_text {
            body.extend_from_slice(b",\"text\":");
            body.extend_from_within(encoded_text_start..encoded_text_end);
        }
        if transport_truncated {
            if !has_complete {
                body.extend_from_slice(b",\"complete\":false");
            }
            if !has_truncated {
                body.extend_from_slice(b",\"truncated\":true");
            }
            if !has_truncation_reason {
                body.extend_from_slice(b",\"truncation_reason\":\"transport_frame_limit\"");
            }
        }
        body.push(b'}');
    }
    body.push(b'}');
    Ok(body)
}

fn response_too_large_frame(
    ver: u8,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    body_len: usize,
    max_body_len: usize,
    include_structured: bool,
) -> Frame {
    let message = format!(
        "tool response serialized to {body_len} bytes, exceeding the daemon transport limit of {max_body_len} bytes; re-run with a narrower range, a smaller limit, or offset+limit paging; output over {} MiB cannot cross the daemon transport",
        transport_limit_mib(max_body_len)
    );
    // Never copy the original response id or data into this backstop. Its bounded
    // fields make the fallback frame independent of the oversized source payload.
    let response = Response::error_with_data(
        format!("subc-{}-{corr}", route.channel),
        RESPONSE_TOO_LARGE_CODE,
        message.clone(),
        serde_json::json!({
            "complete": false,
            "truncated": true,
            "truncation_reason": TRANSPORT_TRUNCATION_REASON,
        }),
    );
    let result = ToolCallResult {
        text: message,
        response,
    };
    let body = serialize_tool_response_body(&result, include_structured)
        .expect("fixed response_too_large envelope must serialize");
    debug_assert!(
        body.len() <= max_body_len,
        "fixed response_too_large envelope must fit the effective body limit"
    );
    Frame::build_with_version(
        ver,
        FrameType::Response,
        flags,
        route.channel,
        route.epoch,
        corr,
        body,
    )
    .expect("fixed response_too_large envelope must fit the protocol frame limit")
}

pub(super) fn build_tool_response_frame(
    ver: u8,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    result: &ToolCallResult,
    trust: BindTrust,
) -> Result<Frame, SubcError> {
    build_tool_response_frame_with_limit(
        ver,
        route,
        corr,
        flags,
        result,
        trust,
        MAX_FRAME_BODY_LEN as usize,
    )
}

pub(super) fn build_tool_response_frame_with_limit(
    ver: u8,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    result: &ToolCallResult,
    trust: BindTrust,
    max_body_len: usize,
) -> Result<Frame, SubcError> {
    // `content`/`isError` is the MCP-native surface a GENERIC host reads. The
    // FIRST-PARTY AFT plugin instead reads `structuredContent`, which carries
    // the full flat standalone shape ({id, success, ...data, text}) so every
    // structured sidecar the plugin drives UI from — status_bar, bg_completions
    // (in-band drain), preview_diff, code, message, attachments — survives the
    // route. subc relays the body byte-for-byte, so this reaches the plugin
    // unchanged. SubcTransport.toolCall re-lifts `structuredContent` straight to
    // the flat ToolCallResult, so nothing downstream of the transport differs
    // from the NDJSON path.
    //
    // UNTRUSTED binds (MCP hosts via subc-mcp) get text-only replies: they
    // have no re-lift layer, we declare no outputSchema (so omitting is
    // MCP-spec-clean), and hosts like Claude Code prefer `structuredContent`
    // for model input when present — feeding the model a raw JSON dump with
    // the rendered text buried inside it, at a multiple of the token cost.
    let include_structured = !matches!(trust, BindTrust::Untrusted);
    let effective_max_body_len = max_body_len.min(MAX_FRAME_BODY_LEN as usize);
    let text_limit = tool_response_text_limit(effective_max_body_len, include_structured);
    let rendered_text = truncate_rendered_text(&result.text, text_limit);
    let transport_truncated = matches!(rendered_text, Cow::Owned(_));
    let body = serialize_tool_response_body_with_text(
        result,
        include_structured,
        &rendered_text,
        transport_truncated,
    )
    .map_err(SubcError::Json)?;

    if effective_max_body_len < MAX_FRAME_BODY_LEN as usize && body.len() > effective_max_body_len {
        return Ok(response_too_large_frame(
            ver,
            route,
            corr,
            flags,
            body.len(),
            effective_max_body_len,
            include_structured,
        ));
    }

    match Frame::build_with_version(
        ver,
        FrameType::Response,
        flags,
        route.channel,
        route.epoch,
        corr,
        body,
    ) {
        Ok(frame) => Ok(frame),
        Err(FrameBuildError::BodyExceedsMax { body_len, max }) => Ok(response_too_large_frame(
            ver,
            route,
            corr,
            flags,
            body_len,
            max as usize,
            include_structured,
        )),
        Err(FrameBuildError::BodyTooLarge { body_len }) => Ok(response_too_large_frame(
            ver,
            route,
            corr,
            flags,
            body_len,
            MAX_FRAME_BODY_LEN as usize,
            include_structured,
        )),
    }
}

pub(super) fn build_error_frame(
    ver: u8,
    channel: u16,
    epoch: u32,
    corr: u64,
    flags: Flags,
    code: &str,
    message: &str,
) -> Result<Frame, SubcError> {
    let body = serde_json::to_vec(&ErrorBody {
        code: code.to_string(),
        message: message.to_string(),
    })
    .map_err(SubcError::Json)?;
    Frame::build_with_version(ver, FrameType::Error, flags, channel, epoch, corr, body)
        .map_err(SubcError::FrameBuild)
}

pub(super) fn build_goodbye_frame(
    ver: u8,
    channel: u16,
    epoch: u32,
    corr: u64,
) -> Result<Frame, SubcError> {
    Frame::build_with_version(
        ver,
        FrameType::Goodbye,
        control_flags(),
        channel,
        epoch,
        corr,
        Vec::new(),
    )
    .map_err(SubcError::FrameBuild)
}

pub(super) fn response_message(response: &Response, fallback: &str) -> String {
    response
        .data
        .get("message")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| fallback.to_string())
}

pub(super) fn response_is_fatal_panic(response: &Response) -> bool {
    !response.success && response.data.get("code").and_then(Value::as_str) == Some("actor_fatal")
}

#[derive(Debug)]
pub enum SubcError {
    Runtime(std::io::Error),
    ConnectionFile {
        path: PathBuf,
        source: subc_transport::ConnectionFileError,
    },
    NoEndpoint {
        path: PathBuf,
    },
    InvalidEndpoint {
        path: PathBuf,
        endpoint: String,
    },
    Connect {
        endpoint: String,
        source: std::io::Error,
    },
    Auth {
        endpoint: String,
        source: subc_transport::AuthError,
    },
    FrameIo(subc_transport::FrameIoError),
    FrameBuild(subc_protocol::FrameBuildError),
    WriterClosed,
    WriterBackpressureTimeout,
    WriterJoin(tokio::task::JoinError),
    Json(serde_json::Error),
    ClosedBeforeHelloAck,
    /// The daemon connection ended (EOF) after attach without a channel-0
    /// Goodbye. Not a stop request: the process must exit non-zero so the
    /// supervisor restarts it, because the supervisor reads exit 0 as "asked
    /// to stop" and never respawns (fleet-wide outage 2026-09-06, 4.5 h).
    ConnectionLost,
    HelloRejected {
        body: Option<ErrorBody>,
    },
    UnexpectedFrame {
        ty: FrameType,
    },
}

impl fmt::Display for SubcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "failed to build subc tokio runtime: {e}"),
            Self::ConnectionFile { path, source } => {
                write!(f, "failed to read subc connection file {path:?}: {source}")
            }
            Self::NoEndpoint { path } => {
                write!(f, "subc connection file {path:?} has no endpoints")
            }
            Self::InvalidEndpoint { path, endpoint } => {
                write!(
                    f,
                    "subc connection file {path:?} has invalid endpoint {endpoint}"
                )
            }
            Self::Connect { endpoint, source } => {
                write!(f, "failed to connect to subc endpoint {endpoint}: {source}")
            }
            Self::Auth { endpoint, source } => {
                write!(
                    f,
                    "failed to authenticate to subc endpoint {endpoint}: {source}"
                )
            }
            Self::FrameIo(e) => write!(f, "subc frame I/O error: {e}"),
            Self::FrameBuild(e) => write!(f, "subc frame build error: {e}"),
            Self::WriterClosed => write!(f, "subc writer task closed"),
            Self::WriterBackpressureTimeout => write!(
                f,
                "subc writer task stayed backpressured while sending a control frame"
            ),
            Self::WriterJoin(e) => write!(f, "subc writer task join error: {e}"),
            Self::Json(e) => write!(f, "subc JSON error: {e}"),
            Self::ClosedBeforeHelloAck => {
                write!(f, "subc daemon closed the connection before HelloAck")
            }
            Self::ConnectionLost => write!(
                f,
                "subc daemon connection ended without a channel-0 Goodbye; exiting for supervisor restart"
            ),
            Self::HelloRejected { body } => match body {
                Some(b) => write!(f, "subc rejected ModuleHello: {} ({})", b.code, b.message),
                None => write!(f, "subc rejected ModuleHello (unparseable error body)"),
            },
            Self::UnexpectedFrame { ty } => {
                write!(f, "subc sent unexpected frame in place of HelloAck: {ty:?}")
            }
        }
    }
}

impl std::error::Error for SubcError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subc::route_key;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use subc_protocol::PROTOCOL_VERSION;

    #[test]
    fn writer_depth_counter_tracks_enqueued_frames_until_drain() {
        let metrics = DispatchPathMetrics::new();
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(8);

        for corr in 1..=3 {
            let frame = Frame::build(FrameType::Ping, control_flags(), 0, 0, corr, Vec::new())
                .expect("test frame");
            assert!(try_enqueue_writer_frame(&writer_tx, &metrics, frame).is_enqueued());
        }
        assert_eq!(metrics.writer_queued.load(Ordering::Relaxed), 3);

        for _ in 0..3 {
            writer_rx.try_recv().expect("queued writer frame");
            decrement_counted_channel(&metrics.writer_queued);
        }
        assert_eq!(metrics.writer_queued.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn reliable_writer_send_retries_after_timeout_and_preserves_frame() {
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(1);
        writer_tx
            .try_send(WriterFrame::plain(
                Frame::build(FrameType::Ping, control_flags(), 0, 0, 1, Vec::new()).unwrap(),
            ))
            .expect("prefill writer queue");

        let metrics_for_task = Arc::clone(&metrics);
        let tx_for_task = writer_tx.clone();
        let send_task = tokio::spawn(async move {
            send_reliable_writer_frame(
                &tx_for_task,
                &metrics_for_task,
                Frame::build(FrameType::Pong, control_flags(), 0, 0, 2, Vec::new()).unwrap(),
                "test reliable frame",
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(2), async {
            while metrics.writer_saturation_count.load(Ordering::Relaxed) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reliable send should observe a timed-out full writer queue");

        let prefilled = writer_rx.recv().await.expect("prefilled frame");
        assert_eq!(prefilled.header.corr, 1);
        let result = tokio::time::timeout(Duration::from_secs(2), send_task)
            .await
            .expect("reliable send should finish after writer drains")
            .expect("reliable send task should not panic");
        assert!(result.is_ok());
        let delivered = writer_rx.recv().await.expect("retried reliable frame");
        assert_eq!(delivered.header.corr, 2);
    }

    #[test]
    fn response_is_fatal_panic_only_matches_panic_exclusive_code() {
        let tool_error = Response::error("request-1", "internal_error", "ordinary tool error");
        let panic_error = Response::error("request-2", "actor_fatal", "mutating panic");

        assert!(!response_is_fatal_panic(&tool_error));
        assert!(response_is_fatal_panic(&panic_error));
    }

    #[tokio::test]
    async fn control_send_times_out_when_writer_queue_remains_full() {
        let (writer_tx, _writer_rx) = mpsc::channel::<WriterFrame>(1);
        let metrics = DispatchPathMetrics::new();
        writer_tx
            .try_send(WriterFrame::plain(
                Frame::build(FrameType::Ping, control_flags(), 0, 0, 1, Vec::new()).unwrap(),
            ))
            .expect("prefill writer queue");
        let started = Instant::now();

        let result = send_frame(
            &writer_tx,
            &metrics,
            Frame::build(FrameType::Pong, control_flags(), 0, 0, 2, Vec::new()).unwrap(),
        )
        .await;

        assert!(matches!(result, Err(SubcError::WriterBackpressureTimeout)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "control send guard should be bounded"
        );
    }

    fn legacy_tool_response_body(result: &ToolCallResult, include_structured: bool) -> Vec<u8> {
        serde_json::to_vec(&ToolResponseEnvelope {
            result,
            include_structured,
        })
        .expect("serialize legacy tool response envelope")
    }

    fn assert_tool_response_frame_matches_legacy(result: &ToolCallResult, trust: BindTrust) {
        let include_structured = !matches!(trust, BindTrust::Untrusted);
        let legacy = Frame::build_with_version(
            PROTOCOL_VERSION,
            FrameType::Response,
            control_flags(),
            7,
            3,
            42,
            legacy_tool_response_body(result, include_structured),
        )
        .expect("build legacy tool response frame");
        let optimized = build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(7, 3),
            42,
            control_flags(),
            result,
            trust,
        )
        .expect("build optimized tool response frame");
        assert_eq!(optimized.header.encode(), legacy.header.encode());
        assert_eq!(optimized.body, legacy.body);
    }

    fn production_shape_result(tool: &str, payload_bytes: usize) -> ToolCallResult {
        let payload = "x".repeat(payload_bytes);
        let response = match tool {
            "read" => Response::success(
                "subc-7-42",
                json!({
                    "content": payload,
                    "path": "/workspace/src/fixture.rs",
                    "start_line": 1,
                    "end_line": payload_bytes / 40 + 1,
                    "total_lines": payload_bytes / 40 + 1,
                    "truncated": false,
                }),
            ),
            "edit" => Response::success(
                "subc-7-42",
                json!({
                    "path": "/workspace/src/fixture.rs",
                    "edits_applied": 1,
                    "diff": { "additions": 12, "deletions": 8 },
                    "preview_diff": payload,
                }),
            ),
            "bash" => Response::success(
                "subc-7-42",
                json!({
                    "output": payload,
                    "exit_code": 0,
                    "timed_out": false,
                    "status": "completed",
                }),
            ),
            _ => unreachable!("production-shape probe tool"),
        };
        let text = crate::subc_format::format_response_with_context(
            tool,
            &response,
            &crate::subc_format::FormatContext::default(),
        );
        ToolCallResult { text, response }
    }

    #[test]
    fn optimized_tool_response_body_matches_legacy_wire_at_production_shapes() {
        for tool in ["read", "edit", "bash"] {
            for payload_bytes in [1_024, 10 * 1_024, 50 * 1_024] {
                let result = production_shape_result(tool, payload_bytes);
                for trust in [BindTrust::Untrusted, BindTrust::FirstParty] {
                    assert_tool_response_frame_matches_legacy(&result, trust);
                }
            }
        }

        let result = ToolCallResult {
            text: r#"replacement text with \"escapes\"
"#
            .to_string(),
            response: Response::success(
                "outer-id",
                json!({
                    "id": "data-id",
                    "success": false,
                    "text": "ignored source text",
                    "nested": [null, true, -7, "replacement text with \"escapes\"\n"],
                }),
            ),
        };
        assert_tool_response_frame_matches_legacy(&result, BindTrust::FirstParty);
    }

    fn median_duration(samples: &mut [Duration]) -> Duration {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    fn time_envelope_builds(
        results: &[ToolCallResult],
        weights: &[usize],
        legacy: bool,
        iterations: usize,
    ) -> Duration {
        let started = Instant::now();
        for _ in 0..iterations {
            for (result, &weight) in results.iter().zip(weights) {
                for _ in 0..weight {
                    let body = if legacy {
                        legacy_tool_response_body(std::hint::black_box(result), true)
                    } else {
                        serialize_tool_response_body(std::hint::black_box(result), true)
                            .expect("serialize optimized tool response envelope")
                    };
                    std::hint::black_box(body);
                }
            }
        }
        started.elapsed()
    }

    #[test]
    #[ignore = "manual release-mode serving-plane performance probe"]
    fn tool_response_envelope_perf_probe() {
        let shapes = [
            ("read", 1_024),
            ("read", 10 * 1_024),
            ("read", 50 * 1_024),
            ("edit", 1_024),
            ("edit", 10 * 1_024),
            ("edit", 50 * 1_024),
            ("bash", 1_024),
            ("bash", 10 * 1_024),
            ("bash", 50 * 1_024),
        ];
        // A 100-call production mix dominated by read/edit/bash, with periodic
        // 50 KiB responses and most calls in the 1-10 KiB range.
        let weights = [20, 15, 5, 15, 10, 5, 15, 10, 5];
        let results: Vec<_> = shapes
            .iter()
            .map(|&(tool, payload_bytes)| production_shape_result(tool, payload_bytes))
            .collect();
        for result in &results {
            assert_eq!(
                serialize_tool_response_body(result, true).unwrap(),
                legacy_tool_response_body(result, true)
            );
        }

        let iterations = 40;
        let calls_per_sample = iterations * weights.iter().sum::<usize>();
        let mut legacy_samples = Vec::new();
        let mut optimized_samples = Vec::new();
        for _ in 0..7 {
            legacy_samples.push(time_envelope_builds(&results, &weights, true, iterations));
            optimized_samples.push(time_envelope_builds(&results, &weights, false, iterations));
        }
        let legacy = median_duration(&mut legacy_samples);
        let optimized = median_duration(&mut optimized_samples);
        let legacy_us = legacy.as_secs_f64() * 1_000_000.0 / calls_per_sample as f64;
        let optimized_us = optimized.as_secs_f64() * 1_000_000.0 / calls_per_sample as f64;
        let saved_percent = (legacy_us - optimized_us) / legacy_us * 100.0;
        eprintln!(
            "trusted mixed tool-response envelope: before={legacy_us:.3} us/call after={optimized_us:.3} us/call saved={saved_percent:.1}%; 7 run medians, {calls_per_sample} calls/run"
        );
        assert!(optimized < legacy, "optimized envelope regressed");
    }

    #[test]
    fn tool_response_frame_carries_flat_standalone_shape_in_structured_content() {
        use crate::protocol::Response;

        // A response with sidecars the FIRST-PARTY plugin drives UI from
        // (status_bar, bg_completions, code) plus a normal result field.
        let response = Response::success(
            "req-7",
            json!({
                "complete": true,
                "matches": 3,
                "status_bar": { "errors": 0, "warnings": 1 },
                "bg_completions": [{ "task_id": "bash-abc" }],
            }),
        );
        let result = ToolCallResult {
            text: "rendered text".to_string(),
            response,
        };

        // The flat shape must equal the standalone NDJSON `tool_call` body:
        // {id, success, ...data, text}. Build the standalone expectation the
        // same way commands::tool_call::response_with_text does.
        let expected_flat = json!({
            "id": "req-7",
            "success": true,
            "complete": true,
            "matches": 3,
            "status_bar": { "errors": 0, "warnings": 1 },
            "bg_completions": [{ "task_id": "bash-abc" }],
            "text": "rendered text",
        });
        assert_eq!(
            serde_json::to_value(FlatToolResponse {
                response: &result.response,
                text: &result.text,
            })
            .unwrap(),
            expected_flat,
            "structuredContent must be byte-identical to the standalone flat response"
        );

        // The frame body carries the MCP surface for generic hosts AND the flat
        // sidecar shape under structuredContent for the first-party plugin.
        let frame = build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            42,
            control_flags(),
            &result,
            BindTrust::FirstParty,
        )
        .unwrap();
        let expected_body = serde_json::to_vec(&json!({
            "content": [{ "type": "text", "text": "rendered text" }],
            "isError": false,
            "structuredContent": expected_flat.clone(),
        }))
        .unwrap();
        assert_eq!(
            frame.body, expected_body,
            "tool response wire bytes drifted"
        );
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["isError"], json!(false));
        assert_eq!(body["content"][0]["type"], json!("text"));
        assert_eq!(body["content"][0]["text"], json!("rendered text"));
        assert_eq!(body["structuredContent"], expected_flat);

        // A failed response flips isError and still carries the flat shape
        // (with success:false + code) for the plugin's error path.
        let err = Response::error_with_data(
            "req-8",
            "ambiguous_match",
            "batch: edits[0] match 'same' is ambiguous (2 occurrences, expected 1). Use 'occurrence' (1-based) to select one, or 'replaceAll': true to replace every occurrence.",
            json!({
                "occurrences": [
                    { "occurrence": 1, "line": 1, "context": "same same" },
                    { "occurrence": 2, "line": 1, "context": "same same" }
                ]
            }),
        );
        let err_result = ToolCallResult {
            text: "batch: edits[0] match 'same' is ambiguous (2 occurrences, expected 1). Use 'occurrence' (1-based) to select one, or 'replaceAll': true to replace every occurrence.".to_string(),
            response: err,
        };
        let err_frame = build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            43,
            control_flags(),
            &err_result,
            BindTrust::FirstParty,
        )
        .unwrap();
        let err_body: Value = serde_json::from_slice(&err_frame.body).unwrap();
        assert_eq!(err_body["isError"], json!(true));
        assert_eq!(err_body["structuredContent"]["success"], json!(false));
        assert_eq!(
            err_body["structuredContent"]["code"],
            json!("ambiguous_match")
        );
        assert_eq!(
            err_body["structuredContent"]["occurrences"],
            json!([
                { "occurrence": 1, "line": 1, "context": "same same" },
                { "occurrence": 2, "line": 1, "context": "same same" }
            ])
        );
        let err_message = err_body["structuredContent"]["message"]
            .as_str()
            .expect("structured contract message");
        assert!(err_message.contains("occurrence"));
        assert!(err_message.contains("1-based"));
        assert!(!err_message.contains("0-based"));
        assert!(!err_message.contains("0-indexed"));
        assert_eq!(err_body["structuredContent"]["text"], json!(err_message));

        // UNTRUSTED (MCP) binds get text-only replies: no structuredContent
        // key at all. Generic MCP hosts have no re-lift layer, and hosts like
        // Claude Code feed structuredContent to the model verbatim when
        // present, a raw JSON dump at a multiple of the token cost.
        let untrusted_frame = build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            44,
            control_flags(),
            &err_result,
            BindTrust::Untrusted,
        )
        .unwrap();
        let untrusted_body: Value = serde_json::from_slice(&untrusted_frame.body).unwrap();
        let untrusted_message = untrusted_body["content"][0]["text"]
            .as_str()
            .expect("untrusted contract message");
        assert!(untrusted_message.contains("occurrence"));
        assert!(untrusted_message.contains("1-based"));
        assert!(!untrusted_message.contains("0-based"));
        assert!(!untrusted_message.contains("0-indexed"));
        assert_eq!(untrusted_body["isError"], json!(true));
        assert!(
            untrusted_body.get("structuredContent").is_none(),
            "untrusted binds must not receive structuredContent: {untrusted_body}"
        );
    }

    #[test]
    fn normal_response_is_byte_identical_with_the_size_guard_enabled() {
        let result = production_shape_result("read", 1_024);
        let default = build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(5, 2),
            76,
            control_flags(),
            &result,
            BindTrust::FirstParty,
        )
        .expect("default response frame");
        let guarded = build_tool_response_frame_with_limit(
            PROTOCOL_VERSION,
            route_key(5, 2),
            76,
            control_flags(),
            &result,
            BindTrust::FirstParty,
            8 * 1_024,
        )
        .expect("guarded response frame");

        assert_eq!(guarded.header.encode(), default.header.encode());
        assert_eq!(guarded.body, default.body);
    }

    #[test]
    fn oversized_rendered_text_is_utf8_safely_truncated_with_an_explicit_gap() {
        const TEST_BODY_LIMIT: usize = 8 * 1_024;
        let text = "é".repeat(3_000);
        let result = ToolCallResult {
            text: text.clone(),
            response: Response::success("large-text", json!({ "text": text })),
        };

        let frame = build_tool_response_frame_with_limit(
            PROTOCOL_VERSION,
            route_key(5, 2),
            77,
            control_flags(),
            &result,
            BindTrust::FirstParty,
            TEST_BODY_LIMIT,
        )
        .expect("truncated response frame");

        assert!(frame.body.len() <= TEST_BODY_LIMIT);
        let body: Value =
            serde_json::from_slice(&frame.body).expect("valid truncated response JSON");
        let rendered = body["content"][0]["text"]
            .as_str()
            .expect("rendered response text");
        assert!(rendered.ends_with(
            "[response truncated at 1 MiB: full output exceeds the transport frame limit; use offset/limit or write to a file]"
        ));
        assert!(rendered.len() < result.text.len());
        assert_eq!(body["isError"], json!(false));
        assert_eq!(body["structuredContent"]["success"], json!(true));
        assert_eq!(body["structuredContent"]["complete"], json!(false));
        assert_eq!(body["structuredContent"]["truncated"], json!(true));
        assert_eq!(
            body["structuredContent"]["truncation_reason"],
            json!(TRANSPORT_TRUNCATION_REASON)
        );
        assert_eq!(body["structuredContent"]["text"], json!(rendered));
    }

    #[test]
    fn oversized_structured_data_gets_a_correlated_response_too_large_fallback() {
        const TEST_BODY_LIMIT: usize = 8 * 1_024;
        let text_limit = tool_response_text_limit(TEST_BODY_LIMIT, true);
        let between_threshold_and_limit = text_limit + 512;
        assert!(between_threshold_and_limit < TEST_BODY_LIMIT);
        let result = ToolCallResult {
            text: "r".repeat(between_threshold_and_limit),
            response: Response::success(
                "large-structured",
                json!({ "payload": "p".repeat(between_threshold_and_limit) }),
            ),
        };

        let frame = build_tool_response_frame_with_limit(
            PROTOCOL_VERSION,
            route_key(9, 4),
            88,
            control_flags(),
            &result,
            BindTrust::FirstParty,
            TEST_BODY_LIMIT,
        )
        .expect("response_too_large fallback frame");

        assert_eq!(frame.header.ty, FrameType::Response);
        assert_eq!(frame.header.channel, 9);
        assert_eq!(frame.header.epoch, 4);
        assert_eq!(frame.header.corr, 88);
        assert!(frame.body.len() <= TEST_BODY_LIMIT);
        let body: Value =
            serde_json::from_slice(&frame.body).expect("valid fallback response JSON");
        assert_eq!(body["isError"], json!(true));
        assert_eq!(body["structuredContent"]["success"], json!(false));
        assert_eq!(
            body["structuredContent"]["code"],
            json!(RESPONSE_TOO_LARGE_CODE)
        );
        assert_eq!(body["structuredContent"]["complete"], json!(false));
        assert_eq!(body["structuredContent"]["truncated"], json!(true));
        let message = body["structuredContent"]["message"]
            .as_str()
            .expect("fallback message");
        assert!(message.contains("serialized to "));
        assert!(message.contains("8192 bytes"));
        assert!(message.contains("narrower range"));
        assert!(message.contains("offset+limit paging"));
        assert!(message.contains("output over 1 MiB cannot cross the daemon transport"));
    }
}
