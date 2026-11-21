//! Display records for runner-facing bash notices, separate from the notice text.

use cortexkit_role_tool_display::{NoticeBody, NoticeWrite, MAX_NOTICE_RECORD_BYTES};
use serde_json::json;
use sha2::{Digest, Sha256};
use subc_protocol::Principal;

use crate::bash_background::persistence::PersistedTask;
use crate::protocol::PushFrame;

mod views;
pub(in crate::subc) use views::{view, view_pins};

pub(crate) const WRITER_CAPABILITY: Option<&str> =
    Some(cortexkit_role_tool_display::NOTICE_RECORD_WRITER_CAPABILITY);
const RECORD_ID_PREFIX: &str = "notice:";

pub(crate) fn is_notice_id(id: &str) -> bool {
    id.starts_with(RECORD_ID_PREFIX)
}

#[derive(Clone, Copy)]
enum Kind {
    Finished,
    StillRunning,
    Watch,
}

impl Kind {
    const ALL: [Self; 3] = [Self::Finished, Self::StillRunning, Self::Watch];
    const VERSION: u64 = 1;

    fn name(self) -> &'static str {
        match self {
            Self::Finished => "bash_finished",
            Self::StillRunning => "bash_still_running",
            Self::Watch => "bash_watch",
        }
    }

    fn data_name(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::StillRunning => "still_running",
            Self::Watch => "watch",
        }
    }
}

fn bounded_prefix(text: &str, cap: usize) -> &str {
    &text[..text.floor_char_boundary(cap.min(text.len()))]
}

fn bounded_tail(text: &str, cap: usize) -> &str {
    &text[text.ceil_char_boundary(text.len().saturating_sub(cap))..]
}

fn body(
    frame: &PushFrame,
    metadata: &PersistedTask,
    now_ms: u64,
    mask: impl Fn(&str) -> String,
) -> Option<NoticeBody> {
    let terminal_duration = metadata.duration_ms.or_else(|| {
        metadata
            .finished_at
            .map(|finish| finish.saturating_sub(metadata.started_at))
    });
    let (kind, status, exit_code, duration_ms, elapsed_ms, preview, cut, watch) = match frame {
        PushFrame::BashCompleted(frame) => (
            Kind::Finished,
            &frame.status,
            frame.exit_code,
            terminal_duration,
            None,
            frame.output_preview.as_str(),
            frame.output_truncated,
            None,
        ),
        PushFrame::BashLongRunning(frame) => (
            Kind::StillRunning,
            &crate::bash_background::BgTaskStatus::Running,
            None,
            None,
            Some(frame.elapsed_ms),
            "",
            false,
            None,
        ),
        PushFrame::BashPatternMatch(frame) => (
            Kind::Watch,
            &metadata.status,
            metadata.exit_code,
            terminal_duration,
            terminal_duration
                .is_none()
                .then(|| now_ms.saturating_sub(metadata.started_at)),
            if frame.reason == "task_exit" {
                frame.context.split_once('\n').map_or("", |(_, tail)| tail)
            } else if !frame.context.is_empty() {
                &frame.context
            } else {
                &frame.match_text
            },
            false,
            Some(frame),
        ),
        _ => return None,
    };
    let task_id = &metadata.task_id;
    let command = crate::log_redact::redact_command(&mask(&metadata.command)).into_owned();
    let description = metadata
        .shell_call
        .as_ref()
        .and_then(|call| call.description.as_deref())
        .filter(|description| !description.trim().is_empty())
        .map(&mask);
    let preview = mask(preview);
    // Bound the other fields as well, including when every byte needs escaping
    // in JSON. If the combined body is too large, give up preview bytes first.
    let mut data = super::records::object(json!({
        "kind": kind.data_name(),
        "task_id": task_id,
        "status": status,
        "command": bounded_prefix(&command, 1536),
    }));
    if let Some(description) = description {
        data.insert(
            "description".into(),
            json!(bounded_prefix(&description, 512)),
        );
    }
    for (field, value) in [
        ("exit_code", json!(exit_code)),
        ("duration_ms", json!(duration_ms)),
        ("elapsed_ms", json!(elapsed_ms)),
    ] {
        if !value.is_null() {
            data.insert(field.into(), value);
        }
    }
    if let Some(watch) = watch {
        data.insert("watch_reason".into(), json!(watch.reason));
        data.insert(
            "watch_match".into(),
            json!(bounded_prefix(&mask(&watch.match_text), 512)),
        );
    }
    fit_preview(
        NoticeBody::new(
            format!("{}:{}", kind.name(), Kind::VERSION),
            (!matches!(kind, Kind::Watch)).then(|| format!("aft:bash-task:{task_id}")),
            data,
        ),
        &preview,
        cut,
    )
}

fn fit_preview(mut body: NoticeBody, preview: &str, cut: bool) -> Option<NoticeBody> {
    let mut cap = 300;
    loop {
        let tail = bounded_tail(preview, cap);
        body.data.insert("output_preview".into(), json!(tail));
        body.data.insert(
            "output_cut".into(),
            json!(cut || tail.len() < preview.len()),
        );
        if body
            .check()
            .is_ok_and(|bytes| bytes.len() <= MAX_NOTICE_RECORD_BYTES)
        {
            return Some(body);
        }
        if cap == 0 {
            return None;
        }
        cap /= 2;
    }
}

pub(crate) use crate::late_result_bus::PullRecord;

pub(super) fn pull(
    runner: &str,
    session: &str,
    send_id: &str,
    body: NoticeBody,
) -> Option<PullRecord> {
    let writer = Principal::Reserved {
        module_id: "aft".into(),
    };
    let write = NoticeWrite::new(
        Principal::Reserved {
            module_id: runner.into(),
        },
        session,
        send_id,
        body,
    );
    write.check(&writer).ok()?;
    let key = cortexkit_role_tool_display::notice_idempotency_key(&writer, &write).ok()?;
    let id = format!("{RECORD_ID_PREFIX}{:x}", Sha256::digest(key));
    let bytes =
        cortexkit_role_tool_display::canonical_body(&super::records::object(json!(write))).ok()?;
    Some(PullRecord { id, bytes })
}

pub(in crate::subc) fn for_send(
    registry: &crate::bash_background::BgTaskRegistry,
    frame: &PushFrame,
    metadata: &PersistedTask,
    runner: &str,
    session: &str,
    send_id: &str,
    now_ms: u64,
) -> Option<PullRecord> {
    WRITER_CAPABILITY?;
    crate::late_result_bus::notice_display_reader()
        .ok()
        .flatten()?;
    let body = body(frame, metadata, now_ms, |text| {
        registry.mask_notice_text(metadata, text)
    })?;
    pull(runner, session, send_id, body)
}

#[cfg(test)]
mod tests;
