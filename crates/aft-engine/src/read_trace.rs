use std::cell::Cell;
use std::fmt::Write;

use serde_json::Value;

use crate::protocol::RawRequest;

#[derive(Clone, Copy)]
struct ReadShape {
    symbols: Option<usize>,
    callgraph: bool,
    file_bytes: Option<u64>,
}

thread_local! {
    static CURRENT: Cell<Option<ReadShape>> = const { Cell::new(None) };
}

pub struct Scope {
    previous: Option<ReadShape>,
    batch_kinds: Option<String>,
}

impl Scope {
    pub fn begin(request: &RawRequest) -> Option<Self> {
        if !matches!(request.command.as_str(), "read" | "outline" | "zoom") {
            return None;
        }
        let zoom = request.command == "zoom";
        let shape = ReadShape {
            symbols: (!zoom).then_some(0),
            callgraph: zoom
                && request.params.get("callgraph").and_then(Value::as_bool) == Some(true),
            file_bytes: None,
        };
        Some(Self {
            previous: CURRENT.replace(Some(shape)),
            batch_kinds: batch_kinds(&request.params),
        })
    }

    pub fn append(self, summary: &mut String) {
        if let Some(shape) = CURRENT.get() {
            if let Some(kinds) = self.batch_kinds.as_deref() {
                let _ = write!(summary, " target_kinds={kinds}");
            }
            let symbols = shape.symbols.map_or_else(|| "-".into(), |n| n.to_string());
            let bytes = shape
                .file_bytes
                .map_or_else(|| "-".into(), |n| n.to_string());
            let _ = write!(
                summary,
                " symbols={symbols} callgraph={} file_bytes={bytes}",
                shape.callgraph
            );
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.set(self.previous);
    }
}

fn batch_kinds(params: &Value) -> Option<String> {
    let files = params.get("files").and_then(Value::as_array);
    let targets = params.get("targets").and_then(Value::as_array);
    let values = files
        .or(targets)
        .filter(|values| values.len() > 1 || files.is_some())?;
    let kinds: std::collections::BTreeSet<_> = values
        .iter()
        .filter_map(|value| {
            value
                .as_str()
                .or_else(|| value.get("file").and_then(Value::as_str))
        })
        .map(|target| {
            if crate::url_fetch::is_http_url(target) {
                "url"
            } else if target.starts_with("issue://") {
                "issue"
            } else if target.starts_with("pr://") {
                "pr"
            } else {
                "file"
            }
        })
        .collect();
    (!kinds.is_empty()).then(|| kinds.into_iter().collect::<Vec<_>>().join(","))
}

/// Use sizes already observed in authorized handlers; the trace never stats or
/// reads a path of its own. For a batch the largest file is most diagnostic.
pub fn note_file(size: u64) {
    if let Some(mut shape) = CURRENT.get() {
        shape.file_bytes = Some(shape.file_bytes.map_or(size, |max| max.max(size)));
        CURRENT.set(Some(shape));
    }
}

pub fn note_symbols(count: usize) {
    if let Some(mut shape) = CURRENT.get() {
        shape.symbols = Some(shape.symbols.unwrap_or(0).saturating_add(count));
        CURRENT.set(Some(shape));
    }
}
