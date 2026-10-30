//! Bounded, trust-independent snapshots of the result used by a keyed reply.
//!
//! The legacy frame already owns the rendered text. References to that text
//! avoid retaining it again in content, output and other matching data fields.
//! Raw JSON field values preserve numbers and field order without reformatting.

use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue, Value};

use crate::run_tool_call::ToolCallResult;

#[derive(Deserialize)]
struct Snapshot {
    version: u8,
    id: String,
    success: bool,
    code: Option<String>,
    text: Option<Box<RawValue>>,
    fields: Vec<(String, Box<RawValue>)>,
    text_fields: Vec<usize>,
}

#[derive(Serialize)]
struct SnapshotRef<'a> {
    version: u8,
    id: &'a str,
    success: bool,
    code: Option<String>,
    text: Option<&'a str>,
    fields: Vec<(&'a str, Option<&'a Value>)>,
    text_fields: Vec<usize>,
}

#[derive(Deserialize)]
struct LegacyText<'a> {
    #[serde(borrow)]
    content: Vec<TextPart<'a>>,
}

#[derive(Deserialize)]
struct TextPart<'a> {
    #[serde(borrow)]
    text: Option<&'a RawValue>,
}

fn legacy_text(body: &[u8]) -> serde_json::Result<Box<RawValue>> {
    let parsed: LegacyText<'_> = serde_json::from_slice(body)?;
    parsed
        .content
        .first()
        .and_then(|part| part.text)
        .map(RawValue::to_owned)
        .ok_or_else(|| {
            <serde_json::Error as serde::de::Error>::custom("legacy reply has no text part")
        })
}

/// Count the whole encoding but retain fewer than `limit` bytes. An oversize
/// encoding is rejected by the caller, never emitted as the retained prefix.
pub(super) struct Encoding {
    pub(super) bytes: Vec<u8>,
    pub(super) size: usize,
    limit: usize,
}

impl Encoding {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            size: 0,
            limit,
        }
    }
    fn raw(&mut self, bytes: &[u8]) {
        self.write_all(bytes)
            .expect("bounded vector writes cannot fail");
    }
}

impl Write for Encoding {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.size = self.size.saturating_add(bytes.len());
        if self.size < self.limit {
            self.bytes.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode(
    result: &ToolCallResult,
    legacy: &[u8],
    limit: usize,
) -> serde_json::Result<Encoding> {
    let legacy_text = legacy_text(legacy)?;
    let inherited: String = serde_json::from_str(legacy_text.get())?;
    let mut fields = Vec::new();
    let mut text_fields = Vec::new();
    if let Some(data) = result.response.data.as_object() {
        for (index, (key, value)) in data.iter().enumerate() {
            let reuse_text = key == "text" || value.as_str() == Some(result.text.as_str());
            if reuse_text {
                text_fields.push(index);
            }
            fields.push((key.as_str(), (!reuse_text).then_some(value)));
        }
    }
    let snapshot = SnapshotRef {
        version: 1,
        id: &result.response.id,
        success: result.response.success,
        code: (!result.response.success)
            .then(|| super::result_body::error_code(result.response.data["code"].as_str())),
        text: (result.text != inherited).then_some(result.text.as_str()),
        fields,
        text_fields,
    };
    let mut output = Encoding::new(limit);
    serde_json::to_writer(&mut output, &snapshot)?;
    Ok(output)
}

pub(super) fn render(
    source: &[u8],
    legacy: &[u8],
    structured: bool,
    limit: usize,
) -> serde_json::Result<Encoding> {
    let snapshot: Snapshot = serde_json::from_slice(source)?;
    if snapshot.version != 1 {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "unsupported retained result version",
        ));
    }
    let inherited = legacy_text(legacy)?;
    let text = snapshot.text.as_deref().unwrap_or(&inherited);
    let mut output = Encoding::new(limit);
    output.raw(b"{\"content\":[{\"type\":\"text\",\"text\":");
    output.raw(text.get().as_bytes());
    output.raw(b"}");
    if let Some(attachments) = snapshot.field("attachments", text) {
        if let Ok(parts) = serde_json::from_str::<Vec<&RawValue>>(attachments.get()) {
            for part in parts {
                let Ok(image) = serde_json::from_str::<std::collections::BTreeMap<String, &RawValue>>(
                    part.get(),
                ) else {
                    continue;
                };
                if image
                    .get("kind")
                    .and_then(|kind| serde_json::from_str::<String>(kind.get()).ok())
                    .as_deref()
                    != Some("image")
                {
                    continue;
                }
                output.raw(b",{\"type\":\"image\"");
                for (wire, native) in [
                    ("data", "data"),
                    ("mimeType", "mime"),
                    ("width", "width"),
                    ("height", "height"),
                    ("alt", "alt"),
                ] {
                    output.raw(b",");
                    serde_json::to_writer(&mut output, wire)?;
                    output.raw(b":");
                    output.raw(
                        image
                            .get(native)
                            .map_or(b"null".as_slice(), |value| value.get().as_bytes()),
                    );
                }
                output.raw(b"}");
            }
        }
    }
    output.raw(b"],\"isError\":");
    output.raw(if snapshot.success { b"false" } else { b"true" });
    if !snapshot.success {
        output.raw(b",\"errorCode\":");
        serde_json::to_writer(
            &mut output,
            &super::result_body::error_code(snapshot.code.as_deref()),
        )?;
    }
    if structured {
        output.raw(b",\"structuredContent\":{\"id\":");
        match snapshot.field("id", text) {
            Some(id) => output.raw(id.get().as_bytes()),
            None => serde_json::to_writer(&mut output, &snapshot.id)?,
        }
        output.raw(b",\"success\":");
        match snapshot.field("success", text) {
            Some(success) => output.raw(success.get().as_bytes()),
            None => serde_json::to_writer(&mut output, &snapshot.success)?,
        }
        for (index, (key, value)) in snapshot.fields.iter().enumerate() {
            if key == "id" || key == "success" {
                continue;
            }
            output.raw(b",");
            serde_json::to_writer(&mut output, key)?;
            output.raw(b":");
            output.raw(if snapshot.text_fields.binary_search(&index).is_ok() {
                text.get().as_bytes()
            } else {
                value.get().as_bytes()
            });
        }
        if snapshot.field("text", text).is_none() {
            output.raw(b",\"text\":");
            output.raw(text.get().as_bytes());
        }
        output.raw(b"}");
    }
    output.raw(b"}");
    Ok(output)
}

impl Snapshot {
    fn field<'a>(&'a self, key: &str, text: &'a RawValue) -> Option<&'a RawValue> {
        self.fields
            .iter()
            .enumerate()
            .find(|(_, (name, _))| name == key)
            .map(|(index, (_, value))| {
                if self.text_fields.binary_search(&index).is_ok() {
                    text
                } else {
                    value.as_ref()
                }
            })
    }
}
