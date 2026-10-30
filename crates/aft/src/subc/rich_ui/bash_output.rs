//! Byte-faithful command output pages. Cursors contain offsets and a read
//! sequence; the MAC binds them to the harness, session and resolved task.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    context::AppContext,
    protocol::{RawRequest, Response},
};

const ITEM_BYTES: usize = 8192;
const PAGE_BYTES: usize = 262_144;
const CURSOR_VERSION: u8 = 1;

#[derive(Clone, Copy, Default)]
struct Offsets {
    stdout: u64,
    stderr: u64,
    read_sequence: u64,
}

struct Cursor<'a> {
    key: [u8; 32],
    harness: &'a str,
    session: &'a str,
    task: &'a str,
}

impl Cursor<'_> {
    fn mac(&self, payload: &[u8]) -> [u8; 32] {
        // HMAC-SHA256 with a fixed 32-byte database key. The JSON tuple keeps
        // identity fields unambiguous even when labels contain separators.
        let mut inner_pad = [0x36; 64];
        let mut outer_pad = [0x5c; 64];
        for (i, byte) in self.key.iter().enumerate() {
            inner_pad[i] ^= byte;
            outer_pad[i] ^= byte;
        }
        let mut inner = Sha256::new();
        inner.update(inner_pad);
        inner.update(
            serde_json::to_vec(&(self.harness, self.session, self.task)).expect("string tuple"),
        );
        inner.update(payload);
        let mut outer = Sha256::new();
        outer.update(outer_pad);
        outer.update(inner.finalize());
        outer.finalize().into()
    }

    fn encode(&self, offsets: Offsets) -> String {
        let mut payload = vec![CURSOR_VERSION];
        payload.extend(offsets.stdout.to_be_bytes());
        payload.extend(offsets.stderr.to_be_bytes());
        payload.extend(offsets.read_sequence.to_be_bytes());
        payload.extend(self.mac(&payload));
        URL_SAFE_NO_PAD.encode(payload)
    }

    fn decode(&self, cursor: &str) -> Option<Offsets> {
        if cursor.len() > 1024 {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(cursor).ok()?;
        if bytes.len() != 57 || bytes[0] != CURSOR_VERSION {
            return None;
        }
        let mac = self.mac(&bytes[..25]);
        let difference = mac
            .iter()
            .zip(&bytes[25..])
            .fold(0u8, |diff, (a, b)| diff | (a ^ b));
        if difference != 0 {
            return None;
        }
        Some(Offsets {
            stdout: u64::from_be_bytes(bytes[1..9].try_into().ok()?),
            stderr: u64::from_be_bytes(bytes[9..17].try_into().ok()?),
            read_sequence: u64::from_be_bytes(bytes[17..25].try_into().ok()?),
        })
    }
}

/// The served-schema check has already accepted `{taskId}`. Native byte
/// offsets are added here, never to the public agent arguments or schema.
pub(super) fn handle(req: &RawRequest, ctx: &AppContext, harness: &str, args: &Value) -> Response {
    let run = || -> Result<Value, String> {
        let db = ctx.db().ok_or("database_unavailable")?;
        let key: Vec<u8> = db
            .lock()
            .map_err(|_| "database_unavailable")?
            .query_row(
                "SELECT key FROM rich_ui_cursor_key WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "database_unavailable")?;
        let params = req.params.get("params").unwrap_or(&req.params);
        let task = params["task_id"].as_str().ok_or("task_not_found")?;
        let cursor = Cursor {
            key: key.try_into().map_err(|_| "database_unavailable")?,
            harness,
            session: req.session(),
            task,
        };
        let offsets = match args.get("cursor").and_then(Value::as_str) {
            Some(value) => cursor.decode(value).ok_or("malformed_cursor")?,
            None => Offsets::default(),
        };
        let mut request = req.clone();
        let params = if request.params.get("params").is_some() {
            &mut request.params["params"]
        } else {
            &mut request.params
        };
        params["output_offset"] = offsets.stdout.into();
        params["stderr_offset"] = offsets.stderr.into();
        let response =
            crate::commands::bash_status::handle_with_observation_only(&request, ctx, true);
        if !response.success {
            return Err(response.data["code"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned());
        }
        page(
            &cursor,
            offsets,
            &response.data,
            args["limit"].as_u64().unwrap_or(200) as usize,
        )
        .map_err(str::to_owned)
    };
    match run() {
        Ok(page) => Response::success(&req.id, page),
        Err(code) => Response::error(&req.id, &code, super::operations::fixed_message(&code)),
    }
}

struct Stream {
    name: &'static str,
    bytes: Vec<u8>,
    more: bool,
}

fn page(
    cursor: &Cursor<'_>,
    mut offsets: Offsets,
    data: &Value,
    limit: usize,
) -> Result<Value, &'static str> {
    let terminal = data["status"].clone();
    let terminal = serde_json::from_value::<crate::bash_background::BgTaskStatus>(terminal)
        .map_err(|_| "result_schema_mismatch")?
        .is_terminal();
    let pty = data["mode"] == "pty";
    // Persisted tasks do not record artifact origins. A terminal/legacy bundle
    // may already have lost a prefix, so never invent an original offset for it.
    let prefix_unknown = terminal || data["output_truncated"] != false;
    let read = |name, field: &str, more: &str| -> Result<Stream, &'static str> {
        Ok(Stream {
            name,
            bytes: base64::engine::general_purpose::STANDARD
                .decode(data[field].as_str().ok_or("result_schema_mismatch")?)
                .map_err(|_| "result_schema_mismatch")?,
            more: data[more].as_bool().ok_or("result_schema_mismatch")?,
        })
    };
    let stdout = read(
        if pty { "pty" } else { "stdout" },
        "output_chunk_base64",
        "output_has_more",
    )?;
    let stderr = if pty {
        Stream {
            name: "stderr",
            bytes: Vec::new(),
            more: false,
        }
    } else {
        read("stderr", "stderr_chunk_base64", "stderr_has_more")?
    };
    if pty {
        offsets.stderr = 0;
    }
    let mut items = Vec::new();
    let mut consumed = [0usize; 2];
    let streams = [stdout, stderr];
    for (index, stream) in streams.iter().enumerate() {
        for (text, bytes) in stream_items(&stream.bytes, terminal && !stream.more) {
            if items.len() == limit {
                return Ok(envelope(cursor, offsets, items, false));
            }
            let mut advanced = offsets;
            if index == 0 {
                advanced.stdout += bytes as u64;
            } else {
                advanced.stderr += bytes as u64;
            }
            let item = json!({"stream":stream.name, "text":text, "bytes":bytes,
                "continued":false, "firstAvailableOffset":if prefix_unknown { None } else { Some(0) },
                "prefixDropped":prefix_unknown});
            items.push(item);
            // Budget the complete serialized page, including JSON escaping and
            // the cursor (or null when this item drains both terminal streams).
            let candidate_drained = terminal
                && streams.iter().enumerate().all(|(i, stream)| {
                    !stream.more
                        && consumed[i] + if i == index { bytes } else { 0 } == stream.bytes.len()
                });
            let candidate = envelope(cursor, advanced, items.clone(), candidate_drained);
            if serde_json::to_vec(&candidate)
                .map_err(|_| "result_schema_mismatch")?
                .len()
                > PAGE_BYTES
            {
                items.pop();
                return Ok(envelope(cursor, offsets, items, false));
            }
            consumed[index] += bytes;
            offsets = advanced;
        }
    }
    let drained = terminal
        && streams
            .iter()
            .enumerate()
            .all(|(i, stream)| !stream.more && consumed[i] == stream.bytes.len());
    Ok(envelope(cursor, offsets, items, drained))
}

fn envelope(cursor: &Cursor<'_>, mut offsets: Offsets, items: Vec<Value>, drained: bool) -> Value {
    // Advance the read sequence even when a running task has no new output,
    // so each read gets a distinct cursor.
    offsets.read_sequence = offsets.read_sequence.wrapping_add(1);
    json!({"items":items, "next":if drained { None } else { Some(cursor.encode(offsets)) }})
}

/// Match `from_utf8_lossy`'s maximal invalid subparts. An unfinished suffix is
/// retained until more bytes arrive or the terminal stream reaches its end.
fn stream_items(bytes: &[u8], finished: bool) -> Vec<(String, usize)> {
    let mut items = Vec::new();
    let mut text = String::new();
    let mut source = 0;
    let mut add = |unit: &str, count: usize| {
        if text.len() + unit.len() > ITEM_BYTES || source + count > ITEM_BYTES {
            items.push((std::mem::take(&mut text), source));
            source = 0;
        }
        text.push_str(unit);
        source += count;
    };
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let (valid, invalid) = match std::str::from_utf8(remaining) {
            Ok(valid) => (valid, None),
            Err(error) => (
                std::str::from_utf8(&remaining[..error.valid_up_to()]).expect("valid prefix"),
                Some(error.error_len()),
            ),
        };
        for character in valid.chars() {
            let mut buffer = [0; 4];
            add(character.encode_utf8(&mut buffer), character.len_utf8());
        }
        remaining = &remaining[valid.len()..];
        match invalid {
            None => break,
            Some(None) if !finished => break,
            Some(length) => {
                let count = length.unwrap_or(remaining.len());
                add("\u{fffd}", count);
                remaining = &remaining[count..];
            }
        }
    }
    if !text.is_empty() {
        items.push((text, source));
    }
    items
}

#[cfg(test)]
#[path = "bash_output/tests.rs"]
mod tests;
