//! Text-only admission for session reads, inside the carried call's policy.

use std::io::Read;
use std::path::Path;

use serde_json::Value;

use crate::{context::AppContext, protocol::Response};

pub(super) fn preflight(
    ctx: &AppContext,
    id: &str,
    session: &str,
    name: &str,
    args: &Value,
) -> Result<(), Response> {
    let targets: Vec<&str> = match name {
        "read" => args["path"].as_str().into_iter().collect(),
        "outline" => args["target"].as_str().into_iter().collect(),
        "zoom" => args["batch"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["target"].as_str())
            .collect(),
        _ => return Ok(()),
    };
    for target in targets {
        // Authorization must precede even a header read: otherwise the media
        // refusal would disclose the type of a file outside the session fence.
        let path = ctx.validate_read_path(id, session, Path::new(target))?;
        let kind =
            crate::bounded_io::run(&path, None, |path| file_kind(&path)).map_err(|error| {
                Response::error(
                    id,
                    crate::bounded_io::timeout_code(&error),
                    "File inspection failed.",
                )
            })?;
        if let Some(code) = kind.and_then(|kind| super::operations::media_refusal(name, kind)) {
            return Err(Response::error(
                id,
                code,
                super::operations::fixed_message(code),
            ));
        }
    }
    Ok(())
}

fn file_kind(path: &Path) -> std::io::Result<Option<&'static str>> {
    if !std::fs::metadata(path)?.is_file() {
        return Ok(None);
    }
    let mut file = crate::task_temp_reads::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(None);
    }
    let mut bytes = [0u8; 8192];
    let len = metadata.len().min(bytes.len() as u64) as usize;
    file.read_exact(&mut bytes[..len])?;
    let prefix = &bytes[..len];
    // Match the native read handler's content signatures, not extensions:
    // even a text-looking GIF or PDF must not become a text-only success.
    if prefix.starts_with(b"\x89PNG\r\n\x1a\n")
        || prefix.starts_with(b"\xff\xd8\xff")
        || prefix.starts_with(b"GIF8")
        || (prefix.len() >= 12 && prefix.starts_with(b"RIFF") && &prefix[8..12] == b"WEBP")
    {
        return Ok(Some("image"));
    }
    if prefix.starts_with(b"%PDF-") {
        return Ok(Some("pdf"));
    }
    if content_inspector::inspect(prefix).is_binary() {
        return Ok(Some("binary"));
    }
    // Invalid UTF-8 beyond the sniffed prefix must not become a binary summary
    // or a lossy text success. Scan in fixed space under the call's I/O budget,
    // carrying an incomplete character into the next read rather than refusing
    // valid text whose character happens to cross a buffer boundary.
    let mut len = len;
    loop {
        let carry = match std::str::from_utf8(&bytes[..len]) {
            Ok(_) => 0,
            Err(error) if error.error_len().is_some() => return Ok(Some("binary")),
            Err(error) => len - error.valid_up_to(),
        };
        bytes.copy_within(len - carry..len, 0);
        match file.read(&mut bytes[carry..]) {
            Ok(0) => return Ok((carry != 0).then_some("binary")),
            Ok(read) => len = carry + read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => len = carry,
            Err(error) => return Err(error),
        }
    }
}
