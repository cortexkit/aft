//! Agent-facing tool → native command translation (subc edge only).

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, PartialEq)]
pub struct Translated {
    pub command: String,
    pub args: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranslateContext {
    pub diagnostics_on_edit: bool,
    pub preview: bool,
    pub effective_hashline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslateError {
    pub code: &'static str,
    pub message: String,
}

/// Largest `topK` an agent may request from `aft_search`. Seven days of local
/// telemetry (7,935 calls, 2026-09) showed rows past rank 50 reaching the
/// session's next three tool calls in about 1% of searches, while topK:100
/// replies cost roughly four times a default one and were 31% of all search
/// output tokens. The deep tail stays reachable through `offset`, which is the
/// cheap shape for "the next ten" rather than re-running retrieval.
pub const SEARCH_MAX_TOP_K: i64 = 50;
pub const SEARCH_TOP_K_BOUNDS_MESSAGE: &str = "topK must be between 1 and 50";

fn invalid_request(message: impl Into<String>) -> TranslateError {
    TranslateError {
        code: "invalid_request",
        message: message.into(),
    }
}

fn path_string<'a>(value: Option<&'a Value>, property: &str) -> Result<&'a str, TranslateError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            invalid_request(format!(
                "'{property}' must be a non-empty well-formed Unicode string"
            ))
        })
}

fn normalize_path_alias_pair(
    map: &mut Map<String, Value>,
    canonical: &str,
    legacy: &str,
    required: bool,
) -> Result<(), TranslateError> {
    // An empty-string or null value on a path-shaped field means the field was
    // not supplied in substance. Strip it before presence counting so hosts
    // that serialize unused optional fields (e.g. `path: ""`) do not trip
    // validation, and a required field that is only an empty sentinel reports
    // the missing-required error rather than the well-formed-Unicode one.
    if is_null_or_empty_string(map.get(canonical)) {
        map.remove(canonical);
    }
    if is_null_or_empty_string(map.get(legacy)) {
        map.remove(legacy);
    }

    let has_canonical = map.contains_key(canonical);
    let has_legacy = map.contains_key(legacy);
    if !has_canonical && !has_legacy {
        if required {
            return Err(invalid_request(format!("'{canonical}' is required")));
        }
        return Ok(());
    }

    if has_canonical && has_legacy {
        let canonical_value = path_string(map.get(canonical), canonical).map(str::to_owned);
        let legacy_value = path_string(map.get(legacy), legacy).map(str::to_owned);
        let (Ok(canonical_value), Ok(legacy_value)) = (canonical_value, legacy_value) else {
            return Err(invalid_request(format!(
                "Invalid request: '{canonical}' and '{legacy}' must both be non-empty well-formed Unicode strings"
            )));
        };
        if canonical_value != legacy_value {
            return Err(invalid_request(format!(
                "Invalid request: '{canonical}' and '{legacy}' must contain equal decoded strings"
            )));
        }
        map.remove(legacy);
        return Ok(());
    }

    if has_canonical {
        path_string(map.get(canonical), canonical)?;
    } else if let Ok(legacy_value) = path_string(map.get(legacy), legacy) {
        map.insert(
            canonical.to_string(),
            Value::String(legacy_value.to_string()),
        );
        map.remove(legacy);
    } else {
        path_string(map.get(legacy), legacy)?;
    }
    Ok(())
}

fn normalize_zoom_target_aliases(target: &mut Value, index: usize) -> Result<(), TranslateError> {
    let Some(object) = target.as_object_mut() else {
        return Err(invalid_request(format!(
            "'targets[{index}].path' must be a non-empty string"
        )));
    };
    // Not required here: an entry with only a symbol still reaches the zoom
    // command, which reports it as its own per-target error line.
    normalize_path_alias_pair(object, "path", "filePath", false)
}

fn parse_stringified_collection(
    value: &mut Value,
    field: &str,
    shape: &str,
    allow_plain_string: bool,
) -> Result<(), TranslateError> {
    if let Value::String(raw) = value {
        let trimmed = raw.trim();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            *value = serde_json::from_str(trimmed).map_err(|_| {
                invalid_request(format!(
                    "{field} must be {shape} (got a string that is not valid JSON)"
                ))
            })?;
        } else if !allow_plain_string {
            return Err(invalid_request(format!(
                "{field} must be {shape} (got a string that is not valid JSON)"
            )));
        }
    }
    Ok(())
}

fn normalize_zoom_aliases(map: &mut Map<String, Value>) -> Result<(), TranslateError> {
    normalize_path_alias_pair(map, "path", "filePath", false)?;
    let Some(targets) = map.get_mut("targets") else {
        return Ok(());
    };
    parse_stringified_collection(targets, "targets", "an array of {path, symbol}", false)?;
    if !targets.is_array() && !targets.is_object() && !targets.is_null() {
        return Err(invalid_request(
            "targets must be an array of {path, symbol} (got an invalid type)",
        ));
    }
    // Entries whose path and symbol are both blank are placeholders from
    // hosts that fill every declared property; they are dropped here, and a
    // `targets` left with no entries counts as not supplied at all.
    let keep = match targets {
        Value::Array(items) => {
            for (index, target) in items.iter_mut().enumerate() {
                if !zoom_target_entry_is_empty(target) {
                    normalize_zoom_target_aliases(target, index)?;
                }
            }
            items.retain(|target| !zoom_target_entry_is_empty(target));
            !items.is_empty()
        }
        Value::Object(_) if zoom_target_entry_is_empty(targets) => false,
        Value::Object(_) => {
            normalize_zoom_target_aliases(targets, 0)?;
            true
        }
        _ => false,
    };
    if !keep {
        map.remove("targets");
    }
    Ok(())
}

fn normalize_path_arguments(bare_name: &str, args: Value) -> Result<Value, TranslateError> {
    let mut map = match args {
        Value::Object(map) => map,
        _ => return Err(invalid_request("tool arguments must be an object")),
    };

    match bare_name {
        "read" | "write" | "move" | "import" => {
            normalize_path_alias_pair(&mut map, "path", "filePath", false)?;
        }
        "edit" => normalize_edit_arguments(&mut map)?,
        "zoom" => normalize_zoom_aliases(&mut map)?,
        "callgraph" => {
            normalize_path_alias_pair(&mut map, "path", "filePath", false)?;
            normalize_path_alias_pair(&mut map, "toPath", "toFile", false)?;
        }
        "safety" => normalize_path_alias_pair(&mut map, "path", "filePath", false)?,
        "grep" | "search" | "conflicts" => {
            // An empty-string or null `path` on these optional-path tools means
            // the field was not supplied; strip it so it cannot trip the
            // well-formed check.
            if is_null_or_empty_string(map.get("path")) {
                map.remove("path");
            } else if map.contains_key("path") {
                path_string(map.get("path"), "path")?;
            }
        }
        _ => {}
    }

    Ok(Value::Object(map))
}

fn normalize_edit_arguments(map: &mut Map<String, Value>) -> Result<(), TranslateError> {
    normalize_edit_path_alias(map)?;

    let supplied_line_fields = ["startLine", "endLine"]
        .into_iter()
        .filter(|key| map.contains_key(*key))
        .collect::<Vec<_>>();
    if !supplied_line_fields.is_empty() {
        let fields = supplied_line_fields
            .iter()
            .map(|field| format!("'{field}'"))
            .collect::<Vec<_>>()
            .join(" and ");
        return Err(invalid_request(format!(
            "edit: top-level {fields} are invalid; line-range fields are valid only inside 'edits[]'. Use edits: [{{ startLine, endLine, content }}]."
        )));
    }

    let unknown_root_keys = map
        .keys()
        .filter(|key| {
            !matches!(
                key.as_str(),
                "path"
                    | "filePath"
                    | "appendContent"
                    | "edits"
                    | "symbol"
                    | "content"
                    | "oldString"
                    | "newString"
                    | "replaceAll"
                    | "occurrence"
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    if !unknown_root_keys.is_empty() {
        return Err(invalid_request(format_unknown_keys(unknown_root_keys)));
    }

    validate_symbol_mode_pair(map)?;
    let modes = edit_modes_present(map);
    if modes.len() > 1 {
        return Err(invalid_request(format!(
            "edit: conflicting modes: {}. Omit unused optional fields entirely; do not send empty strings or empty arrays for them.",
            modes.join(", ")
        )));
    }
    let Some(mode) = modes.first().copied() else {
        return Err(invalid_request(
            "edit: exactly one of `appendContent`, `edits`, or `symbol` plus `content` is required. Omit unused optional fields entirely; do not send empty strings or empty arrays for them.",
        ));
    };

    match mode {
        "appendContent" => {
            if !matches!(map.get("appendContent"), Some(Value::String(_))) {
                return Err(invalid_request("edit: 'appendContent' must be a string"));
            }
        }
        "edits" => {
            let items = parse_edit_array(map.remove("edits"))?;
            let normalized = items
                .into_iter()
                .enumerate()
                .map(|(index, item)| normalize_edit_item(item, index))
                .collect::<Result<Vec<_>, _>>()?;
            map.insert(
                "edits".to_string(),
                Value::Array(normalized.into_iter().map(Value::Object).collect()),
            );
        }
        "symbol/content" => {
            if !matches!(map.get("symbol"), Some(Value::String(_))) {
                return Err(invalid_request(
                    "edit: 'symbol' must be a string when symbol mode is selected",
                ));
            }
            if !matches!(map.get("content"), Some(Value::String(_))) {
                return Err(invalid_request(
                    "edit: incomplete symbol mode: property 'content' must be a string. Retry with `symbol` + `content`, or use `edits[]`.",
                ));
            }
        }
        "oldString/newString" => {
            let mut item = Map::new();
            for key in ["oldString", "newString", "replaceAll", "occurrence"] {
                if let Some(value) = map.get(key) {
                    item.insert(key.to_string(), value.clone());
                }
                map.remove(key);
            }
            let normalized = normalize_edit_item(Value::Object(item), 0)?;
            map.insert(
                "edits".to_string(),
                Value::Array(vec![Value::Object(normalized)]),
            );
        }
        _ => unreachable!("edit mode list contains an unknown mode"),
    }

    let path = map
        .get("path")
        .ok_or_else(|| invalid_request("'path' is required"))?;
    path_string(Some(path), "path")?;
    Ok(())
}

fn normalize_edit_path_alias(map: &mut Map<String, Value>) -> Result<(), TranslateError> {
    // An empty-string or null path means the field was not supplied in
    // substance. Strip it before alias resolution so a legacy-only empty
    // sentinel reports the missing-required error rather than the
    // well-formed-Unicode one.
    if is_null_or_empty_string(map.get("path")) {
        map.remove("path");
    }
    if is_null_or_empty_string(map.get("filePath")) {
        map.remove("filePath");
    }
    let has_path = map.contains_key("path");
    let has_file_path = map.contains_key("filePath");
    match (has_path, has_file_path) {
        (true, true) => normalize_path_alias_pair(map, "path", "filePath", false),
        (false, true) => normalize_path_alias_pair(map, "path", "filePath", false),
        (false, false) | (true, false) => Ok(()),
    }
}

fn validate_symbol_mode_pair(map: &Map<String, Value>) -> Result<(), TranslateError> {
    const COMPLETE_SHAPES: &str = "Retry with `symbol` + `content`, or use `edits[]`.";

    let symbol = map.get("symbol");
    let content = map.get("content");
    if is_non_empty_string(symbol) {
        match content {
            None => {
                return Err(invalid_request(format!(
                    "edit: incomplete symbol mode: missing property 'content'. {COMPLETE_SHAPES}"
                )));
            }
            Some(Value::Null) => {
                return Err(invalid_request(format!(
                    "edit: incomplete symbol mode: property 'content' is null. {COMPLETE_SHAPES}"
                )));
            }
            _ => return Ok(()),
        }
    }

    if is_non_empty_string(content) {
        match symbol {
            None => Err(invalid_request(format!(
                "edit: incomplete symbol mode: missing property 'symbol'. {COMPLETE_SHAPES}"
            ))),
            Some(Value::Null) => Err(invalid_request(format!(
                "edit: incomplete symbol mode: property 'symbol' is null. {COMPLETE_SHAPES}"
            ))),
            _ => Err(invalid_request(format!(
                "edit: incomplete symbol mode: property 'symbol' must be a non-empty string. {COMPLETE_SHAPES}"
            ))),
        }
    } else {
        Ok(())
    }
}

fn edit_modes_present(map: &mut Map<String, Value>) -> Vec<&'static str> {
    // Some hosts serialize every optional field with an empty sentinel. Remove
    // fields that cannot select a mode so later translation cannot revive them.
    let has_append_content = is_non_empty_string(map.get("appendContent"));
    if !has_append_content {
        map.remove("appendContent");
    }

    let has_edits = normalize_edit_array_sentinels(map);
    if !has_edits {
        map.remove("edits");
    }

    let has_symbol = is_non_empty_string(map.get("symbol"));
    if !has_symbol {
        map.remove("symbol");
        if is_null_or_empty_string(map.get("content")) {
            map.remove("content");
        }
    } else if matches!(map.get("content"), Some(Value::Null)) {
        map.remove("content");
    }

    let has_single_edit = is_non_empty_string(map.get("oldString"));
    if !has_single_edit {
        for key in ["oldString", "newString", "replaceAll", "occurrence"] {
            map.remove(key);
        }
    } else {
        for key in ["newString", "replaceAll", "occurrence"] {
            if matches!(map.get(key), Some(Value::Null)) {
                map.remove(key);
            }
        }
    }

    let mut modes = Vec::new();
    if has_append_content {
        modes.push("appendContent");
    }
    if has_edits {
        modes.push("edits");
    }
    if has_symbol {
        modes.push("symbol/content");
    }
    if has_single_edit {
        modes.push("oldString/newString");
    }
    modes
}

fn is_non_empty_string(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::String(value)) if !value.is_empty())
}

fn is_null_or_empty_string(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(value)) => value.is_empty(),
        Some(_) => false,
    }
}

/// An edits item is a serialization sentinel when every payload field carries
/// its type-default value and the real payload lives in a sibling field. Such
/// an item carries no real edit intent and must not claim the `edits` mode.
///
/// A pure line-range item ({startLine,endLine,content}) is never a sentinel,
/// even when `content` is "", because deleting lines is real edit intent. A
/// null `oldString` with a non-null range boundary is treated the same way. A
/// real replacement has a non-empty `oldString`, so it is never a sentinel.
/// `{oldString:"", newString:"non-empty"}` is deliberately NOT a sentinel:
/// it is kept so the batch parser reports its specific empty-match error
/// instead of silently discarding a broken but intentional edit.
fn is_edit_sentinel_item(item: &Value) -> bool {
    let Some(obj) = item.as_object() else {
        return false;
    };
    // `oldString` must be present with an empty-string or null value.
    let old_string_empty =
        obj.contains_key("oldString") && is_null_or_empty_string(obj.get("oldString"));
    if !old_string_empty {
        return false;
    }
    // A non-null range boundary proves that a null oldString belongs to a
    // line-range item, not an all-null serialization sentinel.
    if matches!(obj.get("oldString"), Some(Value::Null))
        && ["startLine", "endLine"]
            .iter()
            .any(|key| !matches!(obj.get(*key), None | Some(Value::Null)))
    {
        return false;
    }
    // Every other payload field must also carry its omitted-value sentinel.
    // Meaningful occurrence or replaceAll values must reach item-family
    // validation instead of being silently discarded.
    is_null_or_empty_string(obj.get("newString"))
        && is_null_or_empty_string(obj.get("content"))
        && matches!(
            obj.get("replaceAll"),
            None | Some(Value::Null) | Some(Value::Bool(false))
        )
        && is_default_occurrence(obj.get("occurrence"))
}

fn has_meaningful_find_payload(item: &Map<String, Value>) -> bool {
    is_non_empty_string(item.get("oldString"))
}

fn is_default_occurrence(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::Number(value)) => value.as_u64() == Some(1),
        _ => false,
    }
}

/// Filter serialization-sentinel items out of the `edits` array (or its
/// stringified form) and rewrite `map["edits"]` to the survivors. Returns
/// whether any real edit items remain, i.e. whether the `edits` mode is still
/// claimed. A non-empty malformed string (or a non-array root) stays an edits
/// claim so the existing parser can report its specific validation error.
fn normalize_edit_array_sentinels(map: &mut Map<String, Value>) -> bool {
    let Some(value) = map.get("edits") else {
        return false;
    };
    match value {
        Value::Array(items) => {
            let survivors: Vec<Value> = items
                .iter()
                .filter(|item| !is_edit_sentinel_item(item))
                .cloned()
                .collect();
            if survivors.is_empty() {
                false
            } else {
                map.insert("edits".to_string(), Value::Array(survivors));
                true
            }
        }
        Value::String(raw) if raw.is_empty() => false,
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Array(items)) => {
                let survivors: Vec<Value> = items
                    .iter()
                    .filter(|item| !is_edit_sentinel_item(item))
                    .cloned()
                    .collect();
                if survivors.is_empty() {
                    false
                } else {
                    map.insert("edits".to_string(), Value::Array(survivors));
                    true
                }
            }
            _ => true,
        },
        _ => false,
    }
}

fn format_unknown_keys(mut keys: Vec<String>) -> String {
    keys.sort();
    format!(
        "Unrecognized keys: {}",
        keys.iter()
            .map(|key| format!("\"{key}\""))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn parse_edit_array(value: Option<Value>) -> Result<Vec<Value>, TranslateError> {
    let Some(value) = value else {
        return Err(invalid_request("edit: 'edits' must be a non-empty array"));
    };
    let value = if let Value::String(raw) = value {
        serde_json::from_str::<Value>(&raw).map_err(|_| {
            invalid_request("edit: 'edits' must contain valid JSON representing an array")
        })?
    } else {
        value
    };
    let Value::Array(items) = value else {
        return Err(invalid_request(
            "edit: 'edits' JSON must have an array root",
        ));
    };
    if items.is_empty() {
        return Err(invalid_request("edit: 'edits' array must not be empty"));
    }
    Ok(items)
}

/// Normalize default fields before selecting an edit family.
///
/// A family whose payload is only omitted-value sentinels yields to the family
/// with meaningful payload. This preserves intentional line-range deletes
/// while allowing hosts that serialize unused fields to submit find/replace
/// requests without a false mixed-mode error.
fn normalize_edit_item_sentinels(item: &mut Map<String, Value>) {
    let had_range_fields = ["startLine", "endLine", "content"]
        .iter()
        .any(|key| item.contains_key(*key));

    // Null is how some hosts serialize an omitted optional property. Remove it
    // before counting either edit family so it cannot create a false conflict.
    for key in [
        "oldString",
        "newString",
        "replaceAll",
        "occurrence",
        "startLine",
        "endLine",
        "content",
    ] {
        if matches!(item.get(key), Some(Value::Null)) {
            item.remove(key);
        }
    }

    let content_is_empty =
        matches!(item.get("content"), Some(Value::String(value)) if value.is_empty());
    if has_meaningful_find_payload(item) && (!item.contains_key("content") || content_is_empty) {
        // A meaningful match wins over blank or absent line-range payload. The
        // boundaries are serializer defaults too when no range content exists.
        for key in ["startLine", "endLine", "content"] {
            item.remove(key);
        }
        // Hosts commonly emit false and 1 alongside an omitted range. They
        // have no effect on a find/replace edit, so keep the established
        // canonical form while preserving meaningful find options.
        if had_range_fields {
            if matches!(item.get("replaceAll"), Some(Value::Bool(false))) {
                item.remove("replaceAll");
            }
            if is_default_occurrence(item.get("occurrence")) && item.contains_key("occurrence") {
                item.remove("occurrence");
            }
        }
        return;
    }

    if !is_non_empty_string(item.get("content")) {
        return;
    }

    // A meaningful range replacement wins over default find/replace fields.
    // Non-default find fields remain so the mixed-mode validator can reject
    // genuinely ambiguous requests.
    if matches!(item.get("oldString"), Some(Value::String(value)) if value.is_empty()) {
        item.remove("oldString");
    }
    if matches!(item.get("newString"), Some(Value::String(value)) if value.is_empty()) {
        item.remove("newString");
    }
    if matches!(item.get("replaceAll"), Some(Value::Bool(false))) {
        item.remove("replaceAll");
    }
    if is_default_occurrence(item.get("occurrence")) && item.contains_key("occurrence") {
        item.remove("occurrence");
    }
}

fn normalize_edit_item(value: Value, index: usize) -> Result<Map<String, Value>, TranslateError> {
    let Value::Object(mut item) = value else {
        return Err(invalid_request(format!(
            "edit: edits[{index}] must be an object"
        )));
    };

    normalize_item_alias(&mut item, "oldString", "oldText");
    normalize_item_alias(&mut item, "newString", "newText");
    let old_string_was_null = item.get("oldString").is_some_and(Value::is_null);
    normalize_edit_item_sentinels(&mut item);

    let has_find = ["oldString", "newString", "replaceAll", "occurrence"]
        .iter()
        .any(|key| item.contains_key(*key));
    let has_range = ["startLine", "endLine", "content"]
        .iter()
        .any(|key| item.contains_key(*key));
    if has_find && has_range {
        return Err(invalid_request(format!(
            "edit: edits[{index}] mixes find/replace and line-range fields"
        )));
    }

    if has_find {
        let problem = match item.get("oldString") {
            Some(Value::String(value)) if value.is_empty() => Some("is empty"),
            Some(Value::String(_)) => None,
            Some(Value::Number(_)) => Some("must be a string (got number)"),
            Some(Value::Bool(_)) => Some("must be a string (got boolean)"),
            Some(Value::Array(_)) => Some("must be a string (got array)"),
            Some(Value::Object(_)) => Some("must be a string (got object)"),
            Some(Value::Null) => Some("is null"),
            None if old_string_was_null => Some("is null"),
            None => Some("is missing"),
        };
        if let Some(problem) = problem {
            return Err(invalid_request(format!(
                "edit: edits[{index}].oldString {problem}"
            )));
        }
        if item.contains_key("newString")
            && !matches!(item.get("newString"), Some(Value::String(_)))
        {
            return Err(invalid_request(format!(
                "edit: edits[{index}].newString must be a string"
            )));
        }
        coerce_edit_scalars(&mut item, index)?;
        validate_edit_item_keys(&item, index)?;
        return Ok(item);
    }

    if has_range {
        for key in ["startLine", "endLine"] {
            let valid = item
                .get(key)
                .and_then(Value::as_u64)
                .is_some_and(|value| value >= 1 && value <= MAX_SAFE_INTEGER as u64);
            if !valid {
                return Err(invalid_request(format!(
                    "edit: edits[{index}].{key} must be a positive integer"
                )));
            }
        }
        let start = item.get("startLine").and_then(Value::as_u64).unwrap();
        let end = item.get("endLine").and_then(Value::as_u64).unwrap();
        if start > end {
            return Err(invalid_request(format!(
                "edit: edits[{index}] requires startLine <= endLine"
            )));
        }
        if !matches!(item.get("content"), Some(Value::String(_))) {
            return Err(invalid_request(format!(
                "edit: edits[{index}] requires string 'content'"
            )));
        }
        validate_edit_item_keys(&item, index)?;
        return Ok(item);
    }

    Err(invalid_request(format!(
        "edit: edits[{index}] must be a find/replace or line-range item"
    )))
}

fn normalize_item_alias(item: &mut Map<String, Value>, canonical: &str, legacy: &str) {
    if let Some(legacy_value) = item.remove(legacy) {
        if !item.contains_key(canonical) {
            item.insert(canonical.to_string(), legacy_value);
        }
    }
}

fn validate_edit_item_keys(item: &Map<String, Value>, index: usize) -> Result<(), TranslateError> {
    let unknown = item
        .keys()
        .filter(|key| {
            !matches!(
                key.as_str(),
                "oldString"
                    | "newString"
                    | "replaceAll"
                    | "occurrence"
                    | "startLine"
                    | "endLine"
                    | "content"
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(invalid_request(format!(
            "edit: edits[{index}] contains {}",
            format_unknown_keys(unknown)
        )))
    }
}

fn coerce_edit_scalars(item: &mut Map<String, Value>, index: usize) -> Result<(), TranslateError> {
    if item.contains_key("replaceAll") && item.contains_key("occurrence") {
        return Err(invalid_request(format!(
            "edit: edits[{index}] cannot contain both 'replaceAll' and 'occurrence'"
        )));
    }
    if let Some(value) = item.get("replaceAll") {
        let coerced = model_boolean(value);
        let Some(coerced) = coerced else {
            return Err(invalid_request(format!(
                "edit: edits[{index}].replaceAll must be a boolean, true/false string, or 0/1"
            )));
        };
        item.insert("replaceAll".to_string(), Value::Bool(coerced));
    }

    if item.contains_key("occurrence") {
        let value = item.get("occurrence").cloned().unwrap();
        match coerce_edit_occurrence(&value, index)? {
            Some(value) => {
                item.insert("occurrence".to_string(), Value::Number(value.into()));
            }
            None => {
                item.remove("occurrence");
            }
        }
    }
    Ok(())
}

fn coerce_edit_occurrence(value: &Value, index: usize) -> Result<Option<u64>, TranslateError> {
    if value.is_null() {
        return Ok(None);
    }
    let parsed = match value {
        Value::Number(number) => number
            .as_u64()
            .filter(|value| *value <= MAX_SAFE_INTEGER as u64)
            .or_else(|| {
                number.as_f64().and_then(|value| {
                    (value.is_finite()
                        && value.fract() == 0.0
                        && value >= 1.0
                        && value <= MAX_SAFE_INTEGER as f64)
                        .then_some(value as u64)
                })
            }),
        Value::String(raw) => {
            let trimmed = raw.trim_matches(|ch: char| ch.is_ascii_whitespace());
            if trimmed.is_empty() {
                return Ok(None);
            }
            let digits = trimmed.strip_prefix('+').unwrap_or(trimmed);
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                None
            } else {
                digits
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value <= MAX_SAFE_INTEGER as u64)
            }
        }
        _ => None,
    };
    match parsed {
        Some(value) if value >= 1 => Ok(Some(value)),
        _ => Err(invalid_request(format!(
            "edit: edits[{index}].occurrence must be a positive integer"
        ))),
    }
}

fn unsupported_tool(message: impl Into<String>) -> TranslateError {
    TranslateError {
        code: "unsupported_tool",
        message: message.into(),
    }
}

fn resolve_home_dir() -> Option<PathBuf> {
    let raw = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(raw)
}

fn expand_tilde(target: &str) -> Cow<'_, str> {
    if target == "~" {
        return resolve_home_dir()
            .map(|h| Cow::Owned(h.to_string_lossy().into_owned()))
            .unwrap_or(Cow::Borrowed(target));
    }
    if let Some(rest) = target.strip_prefix("~/") {
        if let Some(home) = resolve_home_dir() {
            return Cow::Owned(home.join(rest).to_string_lossy().into_owned());
        }
    }
    // Ordinary paths (the overwhelmingly common case) borrow — no allocation.
    Cow::Borrowed(target)
}

/// Decode an RFC 8089 `file:` URL to a local filesystem path.
///
/// Agents (and users pasting editor links) routinely spell local paths as
/// `file:///path`, `file:/path`, or `file://localhost/path`; rejecting those
/// only produces failed tool calls. Accepts empty/`localhost` authorities and
/// percent-decodes the path. A `file://server/share` form (non-local
/// authority) becomes a UNC path on Windows and is left undecoded elsewhere.
/// Grants no extra access: the decoded path flows through the same
/// resolution and permission checks as any literal path — the plugins apply
/// the SAME decoding before their permission gates so both layers judge the
/// identical target.
fn decode_file_url(target: &str) -> Option<String> {
    let rest = target.strip_prefix("file:")?;
    let path_part = if let Some(after) = rest.strip_prefix("//") {
        let (authority, path) = match after.find('/') {
            Some(index) => after.split_at(index),
            None => (after, ""),
        };
        match authority {
            "" | "localhost" => path.to_string(),
            server if cfg!(windows) => format!("//{server}{path}"),
            _ => return None,
        }
    } else {
        // RFC 8089 minimal form: `file:/path` (exactly one slash).
        if !rest.starts_with('/') {
            return None;
        }
        rest.to_string()
    };
    let decoded = percent_decode(&path_part);
    // `file:///C:/path` decodes to `/C:/path`; strip the leading slash so the
    // drive-letter form is a valid Windows absolute path.
    if cfg!(windows) {
        let bytes = decoded.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
        {
            return Some(decoded[1..].to_string());
        }
    }
    Some(decoded)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex_digit = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            if let (Some(high), Some(low)) =
                (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2]))
            {
                let value = high * 16 + low;
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn resolve_path_from_project_root(project_root: &Path, target: &str) -> PathBuf {
    let target = decode_file_url(target)
        .map(std::borrow::Cow::Owned)
        .unwrap_or(std::borrow::Cow::Borrowed(target));
    let expanded = expand_tilde(&target);
    let path = Path::new(expanded.as_ref());
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    normalize_lexically(&joined)
}

fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            Component::Normal(_) | Component::RootDir | Component::Prefix(_) => {
                out.push(component.as_os_str());
            }
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

fn is_empty_param(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn coerce_optional_int_result(
    value: Option<&Value>,
    param_name: &str,
    min: i64,
    max: i64,
) -> Result<Option<u64>, TranslateError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null()
        || matches!(value, Value::String(s) if s.is_empty())
        || matches!(value, Value::Array(a) if a.is_empty())
        || matches!(value, Value::Object(o) if o.is_empty())
    {
        return Ok(None);
    }
    if matches!(value, Value::Number(num) if num.as_i64() == Some(0) && min > 0) {
        return Ok(None);
    }

    let int_error = || {
        invalid_request(format!(
            "{param_name} must be an integer between {min} and {max}"
        ))
    };
    let n = match value {
        Value::Number(num) => num.as_i64().ok_or_else(int_error)?,
        Value::String(s) => {
            let parsed = s.parse::<f64>().map_err(|_| int_error())?;
            if !parsed.is_finite() || parsed.fract() != 0.0 {
                return Err(int_error());
            }
            parsed as i64
        }
        _ => return Err(int_error()),
    };
    if n < min || n > max {
        return Err(invalid_request(format!(
            "{param_name} must be between {min} and {max}"
        )));
    }
    Ok(Some(n as u64))
}

fn agent_args_map(args: Value) -> Map<String, Value> {
    match args {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

pub(crate) fn supports_tool(bare_name: &str) -> bool {
    matches!(
        bare_name,
        "bash"
            | "powershell"
            | "status"
            | "read"
            | "write"
            | "edit"
            | "apply_patch"
            | "grep"
            | "glob"
            | "search"
            | "outline"
            | "zoom"
            | "inspect"
            | "callgraph"
            | "conflicts"
            | "ast_search"
            | "ast_replace"
            | "delete"
            | "move"
            | "import"
            | "safety"
            | "bash_status"
            | "bash_kill"
            | "bash_write"
            | "bash_watch"
    )
}

fn insert_resolved_file(map: &mut Map<String, Value>, project_root: &Path, file_path: &str) {
    let resolved = resolve_path_from_project_root(project_root, file_path);
    map.insert(
        "file".to_string(),
        Value::String(resolved.to_string_lossy().into_owned()),
    );
}

/// A GitHub resource target (`issue://…`, `pr://…`) is a scheme the command
/// handlers route themselves (`is_github_read_target`); resolving it against
/// the project root would turn it into a nonexistent file path. Every tool
/// that advertises the scheme (read, outline, zoom) must go through this.
fn insert_file_or_github_target(
    map: &mut Map<String, Value>,
    project_root: &Path,
    file_path: &str,
) {
    if is_github_resource_target(file_path) {
        map.insert("file".to_string(), Value::String(file_path.to_string()));
    } else {
        insert_resolved_file(map, project_root, file_path);
    }
}

fn is_github_resource_target(target: &str) -> bool {
    crate::commands::github_comments::is_github_resource_path(target)
}

/// Refuses a GitHub address given to a tool that only touches local files.
/// Resolving the address against the project root would turn `issue://7` into
/// `<root>/issue:/7`, which the file handlers would then happily create,
/// move, or delete.
fn reject_github_file_target(tool: &str, target: &str) -> Result<(), TranslateError> {
    if is_github_resource_target(target) {
        return Err(TranslateError {
            code: crate::commands::github_comments::GITHUB_RESOURCE_NOT_FILE,
            message: crate::commands::github_comments::github_resource_not_file_message(
                tool, target,
            ),
        });
    }
    Ok(())
}

pub fn subc_translate(
    bare_name: &str,
    agent_args: &Value,
    project_root: &Path,
) -> Result<Translated, TranslateError> {
    subc_translate_owned(bare_name, agent_args.clone(), project_root)
}

pub fn subc_translate_owned(
    bare_name: &str,
    agent_args: Value,
    project_root: &Path,
) -> Result<Translated, TranslateError> {
    subc_translate_owned_with_context(
        bare_name,
        agent_args,
        project_root,
        TranslateContext::default(),
    )
}

pub fn subc_translate_with_context(
    bare_name: &str,
    agent_args: &Value,
    project_root: &Path,
    ctx: TranslateContext,
) -> Result<Translated, TranslateError> {
    subc_translate_owned_with_context(bare_name, agent_args.clone(), project_root, ctx)
}

pub fn subc_translate_owned_with_context(
    bare_name: &str,
    agent_args: Value,
    project_root: &Path,
    ctx: TranslateContext,
) -> Result<Translated, TranslateError> {
    if bare_name == "edit" && ctx.effective_hashline {
        return crate::hashline::integration::translate_gate_on_edit(&agent_args)
            .map(|translation| {
                let mut args = translation
                    .to_native_args()
                    .as_object()
                    .cloned()
                    .expect("hashline native arguments are always an object");
                if ctx.preview {
                    args.insert("preview".to_string(), Value::Bool(true));
                }
                Translated {
                    command: translation.command.to_string(),
                    args,
                }
            })
            .map_err(|rejection| TranslateError {
                code: rejection.code.as_str(),
                message: format!(
                    "{} at {}: {}\n{}",
                    rejection.code.as_str(),
                    rejection.stage.as_str(),
                    rejection.message,
                    rejection.steering
                ),
            });
    }
    let agent_args = normalize_path_arguments(bare_name, agent_args)?;
    match bare_name {
        "bash" => translate_bash(agent_args, project_root),
        "powershell" => translate_powershell(agent_args, project_root),
        "status" => Ok(Translated {
            command: "status".into(),
            args: Map::new(),
        }),
        "read" => translate_read(agent_args, project_root),
        "write" => translate_write(agent_args, project_root, ctx),
        "edit" => translate_edit(agent_args, project_root, ctx),
        "apply_patch" => translate_apply_patch(agent_args),
        "grep" => translate_grep(agent_args, project_root),
        "glob" => translate_glob(agent_args),
        "search" => translate_search(agent_args),
        "outline" => translate_outline(agent_args, project_root),
        "zoom" => translate_zoom(agent_args, project_root),
        "inspect" => translate_inspect(agent_args, project_root),
        "callgraph" => translate_callgraph(agent_args, project_root),
        "conflicts" => translate_conflicts(agent_args),
        "ast_search" => translate_ast_search(agent_args),
        "ast_replace" => translate_ast_replace(agent_args),
        "delete" => translate_delete(agent_args, project_root),
        "move" => translate_move(agent_args, project_root),
        "import" => translate_import(agent_args),
        "safety" => translate_safety(agent_args, project_root),
        "bash_status" | "bash_kill" | "bash_write" | "bash_watch" => {
            Ok(translate_bash_task_tool(bare_name, agent_args))
        }
        other => Err(unsupported_tool(format!(
            "subc_translate: unsupported tool {other:?}"
        ))),
    }
}

/// Maps the catalog spelling of the bash companion tools (`taskId`,
/// `outputMode`) onto the native snake_case parameters. Every other key passes
/// through untouched, and an explicit snake_case key wins over the camelCase
/// one: AFT's own plugins call these commands natively over the same route
/// with `task_id`, `output_offset` and friends, and must keep working exactly
/// as before.
fn translate_bash_task_tool(bare_name: &str, args: Value) -> Translated {
    fn rename(map: &mut Map<String, Value>, from: &str, to: &str) {
        if let Some(value) = map.remove(from) {
            map.entry(to.to_string()).or_insert(value);
        }
    }
    let mut map = agent_args_map(args);
    rename(&mut map, "taskId", "task_id");
    rename(&mut map, "outputMode", "output_mode");
    rename(&mut map, "timeoutMs", "timeout_ms");
    if let Some(Value::Object(params)) = map.get_mut("params") {
        rename(params, "taskId", "task_id");
        rename(params, "outputMode", "output_mode");
        rename(params, "timeoutMs", "timeout_ms");
    }
    Translated {
        command: bare_name.to_string(),
        args: map,
    }
}

/// Decode model-shaped booleans without treating arbitrary nonempty strings as true.
pub(crate) fn model_boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::Number(num) if num.as_f64() == Some(1.0) => Some(true),
        Value::Number(num) if num.as_f64() == Some(0.0) => Some(false),
        Value::String(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn coerce_boolean(value: &Value) -> bool {
    model_boolean(value).unwrap_or(false)
}

fn translate_bash(args: Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let mut map_in = agent_args_map(args);
    if let Some(Value::Object(params)) = map_in.remove("params") {
        map_in = params;
    }
    let command = map_in
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_request("'command' is required"))?;

    let mut out = Map::new();
    out.insert("command".to_string(), Value::String(command.to_string()));

    if let Some(shell) = map_in.get("shell") {
        if shell.as_str() != Some("powershell") {
            return Err(invalid_request("bash: 'shell' must be 'powershell'"));
        }
        out.insert("shell".to_string(), Value::String("powershell".to_string()));
    }

    if let Some(timeout) =
        coerce_optional_int_result(map_in.get("timeout"), "timeout", 1, MAX_SAFE_INTEGER)?
    {
        out.insert("timeout".to_string(), Value::Number(timeout.into()));
    }

    if let Some(workdir) = map_in
        .get("workdir")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        let resolved = resolve_path_from_project_root(project_root, workdir);
        out.insert(
            "workdir".to_string(),
            Value::String(resolved.to_string_lossy().into_owned()),
        );
    }

    if let Some(description) = map_in
        .get("description")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        out.insert(
            "description".to_string(),
            Value::String(description.to_string()),
        );
    }

    let background = map_in.get("background").is_some_and(coerce_boolean);
    let pty = map_in.get("pty").is_some_and(coerce_boolean);
    let wait = map_in.get("wait").is_some_and(coerce_boolean);
    if wait && pty {
        return Err(invalid_request(
            "bash: wait:true cannot be used with pty:true because PTY sessions run in background",
        ));
    }
    if wait && background {
        return Err(invalid_request(
            "bash: wait:true cannot be used with background:true",
        ));
    }
    out.insert("background".to_string(), Value::Bool(background));
    out.insert("pty".to_string(), Value::Bool(pty));
    out.insert("wait".to_string(), Value::Bool(wait));
    out.insert(
        "notify_on_completion".to_string(),
        Value::Bool(background || pty),
    );

    if let Some(rows) = coerce_optional_int_result(
        map_in.get("ptyRows").or_else(|| map_in.get("pty_rows")),
        "ptyRows",
        1,
        60,
    )? {
        out.insert("pty_rows".to_string(), Value::Number(rows.into()));
    }
    if let Some(cols) = coerce_optional_int_result(
        map_in.get("ptyCols").or_else(|| map_in.get("pty_cols")),
        "ptyCols",
        1,
        140,
    )? {
        out.insert("pty_cols".to_string(), Value::Number(cols.into()));
    }

    if let Some(compressed) = map_in.get("compressed") {
        out.insert(
            "compressed".to_string(),
            Value::Bool(coerce_boolean(compressed)),
        );
    }

    let foreground_orchestrate = map_in
        .get("foreground_orchestrate")
        .map(coerce_boolean)
        .unwrap_or(true);
    let block_to_completion = map_in
        .get("block_to_completion")
        .map(coerce_boolean)
        .unwrap_or(false);
    out.insert(
        "foreground_orchestrate".to_string(),
        Value::Bool(foreground_orchestrate),
    );
    out.insert(
        "block_to_completion".to_string(),
        Value::Bool(block_to_completion),
    );

    if let Some(permissions_granted) = map_in.get("permissions_granted") {
        out.insert(
            "permissions_granted".to_string(),
            permissions_granted.clone(),
        );
    }
    if let Some(permissions_requested) = map_in.get("permissions_requested") {
        out.insert(
            "permissions_requested".to_string(),
            Value::Bool(coerce_boolean(permissions_requested)),
        );
    }
    if let Some(env) = map_in.get("env") {
        out.insert("env".to_string(), env.clone());
    }
    if let Some(sandbox) = map_in.get("sandbox") {
        if sandbox.as_str() != Some("host") {
            return Err(invalid_request("bash: 'sandbox' must be 'host'"));
        }
        out.insert("sandbox".to_string(), sandbox.clone());
    }
    // `runon` is forwarded as written; the bash handler names an unknown
    // demand and decides whether this session may run remotely at all.
    if let Some(runon) = map_in.get("runon").filter(|value| !value.is_null()) {
        let Some(demand) = runon.as_str() else {
            return Err(invalid_request("bash: 'runon' must be a string"));
        };
        out.insert("runon".to_string(), Value::String(demand.to_string()));
    }

    Ok(Translated {
        command: "bash".into(),
        args: out,
    })
}

fn translate_powershell(args: Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let mut translated = translate_bash(args, project_root)?;
    translated
        .args
        .insert("shell".to_string(), Value::String("powershell".to_string()));
    Ok(translated)
}

fn translate_callgraph(args: Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let op = map_in
        .get("op")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("'op' is required"))?;
    if !matches!(
        op,
        "call_tree" | "callers" | "trace_to" | "trace_to_symbol" | "impact" | "trace_data"
    ) {
        return Err(invalid_request(format!("callgraph: invalid op '{op}'")));
    }

    let file_path = map_in
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("'path' is required"))?;
    let symbol = map_in
        .get("symbol")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("'symbol' is required"))?;

    if op == "trace_data" && map_in.get("expression").is_none_or(is_empty_param) {
        return Err(invalid_request(
            "'expression' is required for 'trace_data' op",
        ));
    }
    if op == "trace_to_symbol" && map_in.get("toSymbol").is_none_or(is_empty_param) {
        return Err(invalid_request(
            "'toSymbol' is required for 'trace_to_symbol' op",
        ));
    }

    let mut out = Map::new();
    insert_resolved_file(&mut out, project_root, file_path);
    out.insert("symbol".to_string(), Value::String(symbol.to_string()));

    if let Some(depth) =
        coerce_optional_int_result(map_in.get("depth"), "depth", 1, 9_007_199_254_740_991)?
    {
        out.insert("depth".to_string(), Value::Number(depth.into()));
    }
    if let Some(expression) = map_in.get("expression") {
        if !is_empty_param(expression) {
            out.insert("expression".to_string(), expression.clone());
        }
    }
    if let Some(to_symbol) = map_in.get("toSymbol") {
        if !is_empty_param(to_symbol) {
            out.insert("toSymbol".to_string(), to_symbol.clone());
        }
    }
    if let Some(to_file) = map_in.get("toPath") {
        if !is_empty_param(to_file) {
            let to_file = to_file
                .as_str()
                .ok_or_else(|| invalid_request("'toPath' must be a string"))?;
            let resolved = resolve_path_from_project_root(project_root, to_file);
            out.insert(
                "toFile".to_string(),
                Value::String(resolved.to_string_lossy().into_owned()),
            );
        }
    }
    if let Some(include_tests) = map_in.get("includeTests") {
        if !is_empty_param(include_tests) {
            out.insert(
                "include_tests".to_string(),
                Value::Bool(coerce_boolean(include_tests)),
            );
        }
    }

    Ok(Translated {
        command: op.to_string(),
        args: out,
    })
}

fn insert_common_mutation_flags(out: &mut Map<String, Value>, ctx: TranslateContext) {
    out.insert(
        "diagnostics".to_string(),
        Value::Bool(ctx.diagnostics_on_edit),
    );
    out.insert("include_diff_content".to_string(), Value::Bool(true));
    out.insert("preview".to_string(), Value::Bool(ctx.preview));
}

/// Line window a `read` call resolves to, in the native command's terms.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReadRange {
    start_line: Option<u64>,
    end_line: Option<u64>,
    limit: Option<u64>,
}

/// Resolve `startLine`/`endLine`/`offset`/`limit` into one line window.
///
/// Both plugins and direct daemon callers reach `read` through this function,
/// so it is the one place that decides what the four fields mean together:
///
/// - Some models fill every declared property on every call, sending `0`,
///   `""` or `null` for the ones they do not mean. Those are outside the
///   1-based domain of all four fields, so they count as absent here.
/// - `startLine`/`endLine` take precedence over `offset`/`limit`, matching the
///   documented contract ("offset is ignored if startLine is provided").
/// - `limit` still bounds the window whenever no end line was given.
/// - A start after the end is refused by name. Clamping it (the old
///   behaviour) returned an empty success, which a reader cannot tell apart
///   from "those lines are empty".
fn resolve_read_range(map_in: &Map<String, Value>) -> Result<ReadRange, TranslateError> {
    let int = |key: &str| coerce_optional_int_result(map_in.get(key), key, 1, MAX_SAFE_INTEGER);
    let start_line = int("startLine")?;
    let end_line = int("endLine")?;
    let offset = int("offset")?;
    let limit = int("limit")?;

    let start = start_line.or(offset);
    let end = end_line.or_else(|| {
        // `limit` is a line count from `offset`; it only derives an end line
        // when `offset` is the start actually in use.
        match (start_line, offset, limit) {
            (None, Some(offset), Some(limit)) => Some(offset.saturating_add(limit) - 1),
            _ => None,
        }
    });
    if let (Some(start), Some(end)) = (start, end) {
        if start > end {
            let (start_name, end_name) = (
                if start_line.is_some() {
                    "startLine"
                } else {
                    "offset"
                },
                "endLine",
            );
            return Err(TranslateError {
                code: "invalid_range",
                message: format!(
                    "invalid_range: {start_name} {start} is after {end_name} {end}; no lines selected. Use a start line at or before the end line."
                ),
            });
        }
    }
    Ok(ReadRange {
        start_line: start,
        end_line: end,
        limit: if end.is_none() { limit } else { None },
    })
}

fn translate_read(args: Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let file_path = map_in
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("'path' is required"))?;

    let mut out = Map::new();
    insert_file_or_github_target(&mut out, project_root, file_path);

    let range = resolve_read_range(&map_in)?;
    if let Some(sl) = range.start_line {
        out.insert("start_line".to_string(), Value::Number(sl.into()));
    }
    if let Some(el) = range.end_line {
        out.insert("end_line".to_string(), Value::Number(el.into()));
    }
    if let Some(limit) = range.limit {
        out.insert("limit".to_string(), Value::Number(limit.into()));
    }
    if let Some(vision_capability) = map_in.get("vision_capability").and_then(model_boolean) {
        out.insert(
            "vision_capability".to_string(),
            Value::Bool(vision_capability),
        );
    }

    Ok(Translated {
        command: "read".into(),
        args: out,
    })
}

fn translate_write(
    args: Value,
    project_root: &Path,
    ctx: TranslateContext,
) -> Result<Translated, TranslateError> {
    let mut map_in = agent_args_map(args);
    let file_path = match map_in.remove("path") {
        Some(Value::String(path)) if !path.is_empty() => path,
        _ => return Err(invalid_request("'path' is required")),
    };
    let content = match map_in.remove("content") {
        Some(Value::String(content)) => content,
        _ => return Err(invalid_request("write: missing required param 'content'")),
    };

    let mut out = Map::new();
    // A GitHub address stays unresolved so the write handler posts a comment
    // (or refuses when github.write is off) instead of creating a local file.
    insert_file_or_github_target(&mut out, project_root, &file_path);
    out.insert("content".to_string(), Value::String(content));
    out.insert("create_dirs".to_string(), Value::Bool(true));
    insert_common_mutation_flags(&mut out, ctx);

    Ok(Translated {
        command: "write".into(),
        args: out,
    })
}

fn translate_edit(
    args: Value,
    project_root: &Path,
    ctx: TranslateContext,
) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);

    if map_in.get("startLine").is_some() || map_in.get("endLine").is_some() {
        return Err(invalid_request(
            "edit: 'startLine'/'endLine' are not top-level parameters. \
             For line-range edits, nest them inside the `edits` array. \
             For find/replace, use 'oldString'/'newString'.",
        ));
    }

    let file_path = map_in
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("'path' is required"))?;

    let github_target = is_github_resource_target(file_path);
    // A GitHub address stays unresolved so the edit handlers route it to the
    // comment editor (or refuse it) instead of treating `<root>/issue:/N` as
    // a file.
    let file_str = if github_target {
        file_path.to_string()
    } else {
        resolve_path_from_project_root(project_root, file_path)
            .to_string_lossy()
            .into_owned()
    };

    if let Some(append) = map_in.get("appendContent").and_then(Value::as_str) {
        let mut out = Map::new();
        out.insert("file".to_string(), Value::String(file_str));
        out.insert("op".to_string(), Value::String("append".into()));
        out.insert(
            "append_content".to_string(),
            Value::String(append.to_string()),
        );
        out.insert("create_dirs".to_string(), Value::Bool(true));
        insert_common_mutation_flags(&mut out, ctx);
        return Ok(Translated {
            command: "edit_match".into(),
            args: out,
        });
    }

    if let Some(edits) = map_in.get("edits").and_then(Value::as_array) {
        // A GitHub comment takes exactly one find/replace, which the
        // `edit_match` command applies to the addressed comment. The `batch`
        // command is file-only and must never see a GitHub address.
        if github_target {
            if let [single] = edits.as_slice() {
                if let Some(obj) = single.as_object() {
                    let is_find_replace = obj.get("oldString").is_some_and(Value::is_string)
                        && !obj.contains_key("startLine")
                        && !obj.contains_key("endLine")
                        && !obj.contains_key("content");
                    if is_find_replace {
                        return translate_single_edit_match(obj, file_str, ctx);
                    }
                }
            }
            return Err(invalid_request(format!(
                "edit: '{file_path}' is a GitHub address; GitHub comments accept exactly one \
                 edits[] find/replace entry (oldString and optional newString) on \
                 issue://N/comments/K or pr://N/comments/K. Nothing was changed."
            )));
        }
        // The batch command is single-file only; glob targets are an
        // edit_match capability. A glob path with one find/replace item
        // (the folded single-edit form included) must keep routing to
        // edit_match or glob edits silently break with "file not found".
        if path_is_glob_pattern(file_path) {
            if let [single] = edits.as_slice() {
                if let Some(obj) = single.as_object() {
                    let is_find_replace = obj.contains_key("oldString")
                        && !obj.contains_key("startLine")
                        && !obj.contains_key("endLine");
                    if is_find_replace {
                        return translate_single_edit_match(obj, file_str, ctx);
                    }
                }
            }
            return Err(invalid_request(
                "edit: glob targets support exactly one find/replace edit \
                 (oldString/newString); line-range and multi-item batches \
                 need a concrete file path",
            ));
        }
        let mut out = Map::new();
        out.insert("file".to_string(), Value::String(file_str));
        let translated_edits: Vec<Value> = edits
            .iter()
            .filter_map(|edit| {
                let obj = edit.as_object()?;
                let mut t = Map::new();
                for (key, value) in obj {
                    let native_key = match key.as_str() {
                        "oldString" => "match",
                        "newString" => "replacement",
                        "startLine" => "line_start",
                        "endLine" => "line_end",
                        other => other,
                    };
                    t.insert(native_key.to_string(), value.clone());
                }
                Some(Value::Object(t))
            })
            .collect();
        out.insert("edits".to_string(), Value::Array(translated_edits));
        insert_common_mutation_flags(&mut out, ctx);
        return Ok(Translated {
            command: "batch".into(),
            args: out,
        });
    }

    let symbol_is_string = map_in.get("symbol").and_then(Value::as_str).is_some();
    let old_string_is_string = map_in.get("oldString").and_then(Value::as_str).is_some();
    let has_content = map_in.get("content").is_some();

    if symbol_is_string && !old_string_is_string && has_content {
        let mut out = Map::new();
        out.insert("file".to_string(), Value::String(file_str));
        out.insert(
            "symbol".to_string(),
            map_in.get("symbol").cloned().unwrap_or(Value::Null),
        );
        out.insert("operation".to_string(), Value::String("replace".into()));
        out.insert(
            "content".to_string(),
            map_in.get("content").cloned().unwrap_or(Value::Null),
        );
        insert_common_mutation_flags(&mut out, ctx);
        return Ok(Translated {
            command: "edit_symbol".into(),
            args: out,
        });
    }

    if old_string_is_string {
        return translate_single_edit_match(&map_in, file_str, ctx);
    }

    Err(invalid_request(
        "edit: no edit mode resolved from arguments.",
    ))
}

/// A glob spelling in an edit target (single-file batch is the alternative).
fn path_is_glob_pattern(path: &str) -> bool {
    path.contains('*') || path.contains('?') || path.contains('{') || path.contains('[')
}

/// Route one find/replace edit to the `edit_match` command, which owns both
/// concrete-file and glob targets.
fn translate_single_edit_match(
    fields: &Map<String, Value>,
    file_str: String,
    ctx: TranslateContext,
) -> Result<Translated, TranslateError> {
    let mut out = Map::new();
    out.insert("file".to_string(), Value::String(file_str));
    out.insert(
        "match".to_string(),
        Value::String(
            fields
                .get("oldString")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
    );
    let replacement = fields
        .get("newString")
        .and_then(Value::as_str)
        .unwrap_or("");
    out.insert(
        "replacement".to_string(),
        Value::String(replacement.to_string()),
    );
    if let Some(v) = fields.get("replaceAll") {
        out.insert("replace_all".to_string(), v.clone());
    }
    if let Some(v) = fields.get("occurrence") {
        out.insert("occurrence".to_string(), v.clone());
    }
    insert_common_mutation_flags(&mut out, ctx);
    Ok(Translated {
        command: "edit_match".into(),
        args: out,
    })
}

fn translate_apply_patch(args: Value) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let patch_text = map_in
        .get("patchText")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("apply_patch: missing required param 'patchText'"))?;

    let mut out = Map::new();
    out.insert(
        "patch_text".to_string(),
        Value::String(patch_text.to_string()),
    );
    Ok(Translated {
        command: "apply_patch".into(),
        args: out,
    })
}

fn translate_grep(args: Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let pattern = map_in
        .get("pattern")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("grep: missing required param 'pattern'"))?;

    let mut out = Map::new();
    out.insert("pattern".to_string(), Value::String(pattern.to_string()));
    out.insert("case_sensitive".to_string(), Value::Bool(true));
    if let Some(offset) = coerce_optional_int_result(map_in.get("offset"), "offset", 0, 100_000)? {
        out.insert("offset".to_string(), Value::Number(offset.into()));
    }
    if let Some(include) = map_in.get("include") {
        if !is_empty_param(include) {
            let include_arg = include.as_str().ok_or_else(|| {
                invalid_request("grep: 'include' must be a comma-separated string")
            })?;
            let includes = split_include_arg(include_arg)
                .into_iter()
                .map(|pattern| Value::String(normalize_glob(&pattern)))
                .collect::<Vec<_>>();
            if !includes.is_empty() {
                out.insert("include".to_string(), Value::Array(includes));
            }
        }
    }
    if let Some(path_val) = map_in.get("path") {
        if !is_empty_param(path_val) {
            if let Some(path_str) = path_val.as_str() {
                out.insert(
                    "path".to_string(),
                    Value::String(resolve_grep_path_arg(project_root, path_str)),
                );
            }
        }
    }
    out.insert("max_results".to_string(), Value::Number(100u64.into()));

    Ok(Translated {
        command: "grep".into(),
        args: out,
    })
}

fn translate_ast_search(args: Value) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let pattern = map_in
        .get("pattern")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("ast_search: missing required param 'pattern'"))?;
    let lang = map_in
        .get("lang")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("ast_search: missing required param 'lang'"))?;

    let mut out = Map::new();
    out.insert("pattern".to_string(), Value::String(pattern.to_string()));
    out.insert("lang".to_string(), Value::String(lang.to_string()));
    insert_non_empty_array(&mut out, &map_in, "paths");
    insert_non_empty_array(&mut out, &map_in, "globs");
    if let Some(context) = coerce_optional_int_result(
        map_in.get("contextLines"),
        "contextLines",
        1,
        9_007_199_254_740_991,
    )? {
        out.insert("context".to_string(), Value::Number(context.into()));
    }

    Ok(Translated {
        command: "ast_search".into(),
        args: out,
    })
}

fn translate_ast_replace(args: Value) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let pattern = map_in
        .get("pattern")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("ast_replace: missing required param 'pattern'"))?;
    let rewrite = map_in
        .get("rewrite")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_request("ast_replace: missing required param 'rewrite'"))?;
    let lang = map_in
        .get("lang")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("ast_replace: missing required param 'lang'"))?;
    if let Some(paths) = map_in.get("paths").and_then(Value::as_array) {
        for path in paths.iter().filter_map(Value::as_str) {
            reject_github_file_target("ast_replace", path)?;
        }
    }

    let mut out = Map::new();
    out.insert("pattern".to_string(), Value::String(pattern.to_string()));
    out.insert("rewrite".to_string(), Value::String(rewrite.to_string()));
    out.insert("lang".to_string(), Value::String(lang.to_string()));
    insert_non_empty_array(&mut out, &map_in, "paths");
    insert_non_empty_array(&mut out, &map_in, "globs");
    let dry_run = map_in
        .get("dryRun")
        .or_else(|| map_in.get("dry_run"))
        .is_some_and(coerce_boolean);
    out.insert("dry_run".to_string(), Value::Bool(dry_run));

    Ok(Translated {
        command: "ast_replace".into(),
        args: out,
    })
}

fn insert_present_renamed(
    out: &mut Map<String, Value>,
    map_in: &Map<String, Value>,
    from: &str,
    to: &str,
) {
    if let Some(value) = map_in.get(from) {
        out.insert(to.to_string(), value.clone());
    }
}

fn translate_delete(args: Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let mut map_in = agent_args_map(args);
    if let Some(files) = map_in.get_mut("files") {
        parse_stringified_collection(files, "files", "a non-empty array of paths", false)?;
    }
    let files = map_in
        .get("files")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| invalid_request("delete: 'files' must be a non-empty array of paths"))?;

    let mut resolved_files = Vec::with_capacity(files.len());
    for file in files {
        let file = file
            .as_str()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| invalid_request("delete: 'files' must be a non-empty array of paths"))?;
        reject_github_file_target("delete", file)?;
        let resolved = resolve_path_from_project_root(project_root, file);
        resolved_files.push(Value::String(resolved.to_string_lossy().into_owned()));
    }

    let mut out = Map::new();
    out.insert("files".to_string(), Value::Array(resolved_files));
    out.insert(
        "recursive".to_string(),
        Value::Bool(map_in.get("recursive").is_some_and(coerce_boolean)),
    );

    Ok(Translated {
        command: "delete_file".into(),
        args: out,
    })
}

fn translate_move(args: Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let file_path = map_in
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("aft_move: missing required param 'path'"))?;
    let destination = map_in
        .get("destination")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("aft_move: missing required param 'destination'"))?;

    reject_github_file_target("aft_move", file_path)?;
    reject_github_file_target("aft_move", destination)?;
    let file_path = resolve_path_from_project_root(project_root, file_path);
    let destination = resolve_path_from_project_root(project_root, destination);

    let mut out = Map::new();
    out.insert(
        "file".to_string(),
        Value::String(file_path.to_string_lossy().into_owned()),
    );
    out.insert(
        "destination".to_string(),
        Value::String(destination.to_string_lossy().into_owned()),
    );

    Ok(Translated {
        command: "move_file".into(),
        args: out,
    })
}

fn translate_import(args: Value) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let op = map_in
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_request("aft_import: missing required param 'op'"))?;
    let command = match op {
        "add" => "add_import",
        "remove" => "remove_import",
        "organize" => "organize_imports",
        other => {
            return Err(invalid_request(format!(
                "aft_import: invalid op {other:?}; expected 'add', 'remove', or 'organize'"
            )));
        }
    };

    let file_path = map_in
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("aft_import: missing required param 'filePath'"))?;
    reject_github_file_target("aft_import", file_path)?;

    if matches!(op, "add" | "remove") && map_in.get("module").map_or(true, is_empty_param) {
        return Err(invalid_request(format!(
            "'module' is required for '{op}' op"
        )));
    }

    let mut out = Map::new();
    out.insert("file".to_string(), Value::String(file_path.to_string()));
    insert_present_renamed(&mut out, &map_in, "module", "module");
    insert_present_renamed(&mut out, &map_in, "names", "names");
    insert_present_renamed(&mut out, &map_in, "defaultImport", "default_import");
    insert_present_renamed(&mut out, &map_in, "namespace", "namespace");
    insert_present_renamed(&mut out, &map_in, "alias", "alias");
    insert_present_renamed(&mut out, &map_in, "modifiers", "modifiers");
    insert_present_renamed(&mut out, &map_in, "importKind", "import_kind");
    if let Some(value) = map_in.get("typeOnly") {
        out.insert("type_only".to_string(), Value::Bool(coerce_boolean(value)));
    }
    insert_present_renamed(&mut out, &map_in, "removeName", "name");
    insert_present_renamed(&mut out, &map_in, "validate", "validate");

    Ok(Translated {
        command: command.into(),
        args: out,
    })
}

fn translate_safety(args: Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let op = map_in
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_request("aft_safety: missing required param 'op'"))?;
    let command = match op {
        "undo" => "undo",
        "history" => "edit_history",
        "checkpoint" => "checkpoint",
        "restore" => "restore_checkpoint",
        "list" => "list_checkpoints",
        other => {
            return Err(invalid_request(format!(
                "aft_safety: invalid op {other:?}; expected 'undo', 'history', 'checkpoint', 'restore', or 'list'"
            )));
        }
    };

    if op == "history" && map_in.get("path").and_then(Value::as_str).is_none() {
        return Err(invalid_request("'path' is required for 'history' op"));
    }
    if matches!(op, "checkpoint" | "restore")
        && map_in.get("name").and_then(Value::as_str).is_none()
    {
        return Err(invalid_request(format!("'name' is required for '{op}' op")));
    }

    let resolve_path = |value: &Value| -> Result<Value, TranslateError> {
        let path = value
            .as_str()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| invalid_request("aft_safety: paths must be non-empty strings"))?;
        reject_github_file_target("aft_safety", path)?;
        Ok(Value::String(
            resolve_path_from_project_root(project_root, path)
                .to_string_lossy()
                .into_owned(),
        ))
    };

    let mut out = Map::new();
    insert_present_renamed(&mut out, &map_in, "name", "name");
    // A blank entry in `files` is a placeholder from hosts that fill every
    // declared property (`files: [""]`), not a path; it counts as absent.
    let files = map_in
        .get("files")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| !matches!(item, Value::String(path) if path.trim().is_empty()))
                .collect::<Vec<_>>()
        })
        .filter(|items| !items.is_empty())
        .map(|items| {
            items
                .into_iter()
                .map(resolve_path)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;

    if op == "checkpoint" {
        if let Some(files) = files {
            out.insert("files".to_string(), Value::Array(files));
        } else if let Some(file_path) = map_in.get("path") {
            out.insert(
                "files".to_string(),
                Value::Array(vec![resolve_path(file_path)?]),
            );
        }
    } else {
        if let Some(file_path) = map_in.get("path") {
            out.insert("file".to_string(), resolve_path(file_path)?);
        }
        if let Some(files) = files {
            out.insert("files".to_string(), Value::Array(files));
        }
    }

    Ok(Translated {
        command: command.into(),
        args: out,
    })
}

fn insert_non_empty_array(out: &mut Map<String, Value>, map_in: &Map<String, Value>, key: &str) {
    if let Some(value) = map_in.get(key) {
        if let Some(items) = value.as_array() {
            if !items.is_empty() {
                out.insert(key.to_string(), Value::Array(items.clone()));
            }
        }
    }
}

fn translate_glob(args: Value) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let pattern = map_in
        .get("pattern")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_request("glob: missing required param 'pattern'"))?;

    let mut out = Map::new();
    out.insert("pattern".to_string(), Value::String(pattern.to_string()));
    if let Some(path_val) = map_in.get("path") {
        if !is_empty_param(path_val) {
            if let Some(path_str) = path_val.as_str() {
                out.insert("path".to_string(), Value::String(path_str.to_string()));
            }
        }
    }

    Ok(Translated {
        command: "glob".into(),
        args: out,
    })
}

fn normalize_glob(pattern: &str) -> String {
    if !pattern.contains('/') && !pattern.starts_with("**/") {
        format!("**/{pattern}")
    } else {
        pattern.to_string()
    }
}

fn split_include_arg(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut buf = String::new();
    for ch in raw.chars() {
        match ch {
            '{' => {
                depth += 1;
                buf.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                buf.push(ch);
            }
            ',' if depth == 0 => {
                let trimmed = buf.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                buf.clear();
            }
            _ => buf.push(ch),
        }
    }
    let trimmed = buf.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    out
}

fn search_path_exists(project_root: &Path, raw: &str) -> bool {
    resolve_path_from_project_root(project_root, raw).exists()
}

fn split_search_path_arg(project_root: &Path, raw: &str) -> Vec<String> {
    if search_path_exists(project_root, raw) || !raw.chars().any(char::is_whitespace) {
        return vec![raw.to_string()];
    }

    let fragments = raw
        .split_whitespace()
        .filter(|fragment| !fragment.is_empty())
        .collect::<Vec<_>>();
    if fragments.len() < 2 {
        return vec![raw.to_string()];
    }

    let existing = fragments
        .iter()
        .filter(|fragment| search_path_exists(project_root, fragment))
        .map(|fragment| (*fragment).to_string())
        .collect::<Vec<_>>();
    if existing.is_empty() {
        vec![raw.to_string()]
    } else {
        existing
    }
}

fn resolve_grep_path_arg(project_root: &Path, raw: &str) -> String {
    split_search_path_arg(project_root, raw)
        .iter()
        .map(|target| {
            resolve_path_from_project_root(project_root, target)
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn translate_search(args: Value) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    // `query` is prose (or a single auto-routed string); `pattern` is a regex
    // in grep's syntax. At least one must be non-empty. A whitespace-only
    // pattern counts as absent, but a real pattern is forwarded untrimmed:
    // whitespace inside a regex is part of what it matches.
    let query = map_in
        .get("query")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    let pattern = match map_in.get("pattern") {
        None | Some(Value::Null) => None,
        Some(Value::String(pattern)) => Some(pattern.as_str()).filter(|s| !s.trim().is_empty()),
        Some(_) => {
            return Err(invalid_request(
                "semantic_search: invalid params: `pattern` must be a string",
            ));
        }
    };
    if query.is_none() && pattern.is_none() {
        return Err(invalid_request(if map_in.contains_key("pattern") {
            "semantic_search: invalid params: at least one of `query` or `pattern` must be a non-empty string"
        } else {
            "semantic_search: invalid params: `query` must be a non-empty string"
        }));
    }

    let mut out = Map::new();
    if let Some(query) = query {
        out.insert("query".to_string(), Value::String(query.to_string()));
    }
    if let Some(pattern) = pattern {
        out.insert("pattern".to_string(), Value::String(pattern.to_string()));
    }
    let top_k_value = coerce_optional_int_result(map_in.get("topK"), "topK", 0, SEARCH_MAX_TOP_K)
        .map_err(|error| {
        if error.message.starts_with("topK must be between") {
            invalid_request(SEARCH_TOP_K_BOUNDS_MESSAGE)
        } else {
            error
        }
    })?;
    let top_k = match top_k_value {
        Some(0) => return Err(invalid_request(SEARCH_TOP_K_BOUNDS_MESSAGE)),
        Some(value) => value,
        None => 10,
    };
    out.insert("top_k".to_string(), Value::Number(top_k.into()));
    if let Some(offset) = coerce_optional_int_result(map_in.get("offset"), "offset", 0, 100_000)? {
        out.insert("offset".to_string(), Value::Number(offset.into()));
    }
    if let Some(include_tests) = map_in.get("includeTests") {
        if !is_empty_param(include_tests) {
            out.insert(
                "include_tests".to_string(),
                Value::Bool(coerce_boolean(include_tests)),
            );
        }
    }
    if let Some(path) = map_in
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        out.insert("path".to_string(), Value::String(path.to_string()));
    }

    Ok(Translated {
        command: "semantic_search".into(),
        args: out,
    })
}

fn translate_outline(args: Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let files_flag = map_in.get("files").is_some_and(coerce_boolean);

    let target = map_in
        .get("target")
        .ok_or_else(|| invalid_request("outline: missing required param 'target'"))?;

    if is_empty_param(target) {
        return Err(invalid_request(
            "'target' must be a non-empty string or array of strings",
        ));
    }

    let mut out = Map::new();
    if let Some(include_tests) = map_in
        .get("includeTests")
        .filter(|value| !is_empty_param(value))
        .or_else(|| {
            map_in
                .get("include_tests")
                .filter(|value| !is_empty_param(value))
        })
    {
        out.insert(
            "includeTests".to_string(),
            Value::Bool(coerce_boolean(include_tests)),
        );
    }

    if let Some(arr) = target.as_array() {
        if arr.is_empty() {
            return Err(invalid_request(
                "'target' must be a non-empty string or array of strings",
            ));
        }
        if files_flag {
            let resolved: Vec<Value> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(|entry| {
                    let p = resolve_path_from_project_root(project_root, entry);
                    Value::String(p.to_string_lossy().into_owned())
                })
                .collect();
            out.insert("target".to_string(), Value::Array(resolved));
            out.insert("files".to_string(), Value::Bool(true));
            return Ok(Translated {
                command: "outline".into(),
                args: out,
            });
        }
        let resolved: Vec<Value> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|entry| {
                let p = resolve_path_from_project_root(project_root, entry);
                Value::String(p.to_string_lossy().into_owned())
            })
            .collect();
        out.insert("files".to_string(), Value::Array(resolved));
        return Ok(Translated {
            command: "outline".into(),
            args: out,
        });
    }

    if let Some(url) = target.as_str() {
        if !files_flag
            && (url.starts_with("http://")
                || url.starts_with("https://")
                || is_github_resource_target(url))
        {
            out.insert("file".to_string(), Value::String(url.to_string()));
            return Ok(Translated {
                command: "outline".into(),
                args: out,
            });
        }
    }

    let target_str = target.as_str().ok_or_else(|| {
        invalid_request("'target' must be a non-empty string or array of strings")
    })?;

    let resolved = resolve_path_from_project_root(project_root, target_str);
    let is_dir = std::fs::metadata(&resolved)
        .map(|m| m.is_dir())
        .unwrap_or(false);

    if files_flag {
        if is_dir {
            out.insert(
                "directory".to_string(),
                Value::String(resolved.to_string_lossy().into_owned()),
            );
        } else {
            out.insert(
                "file".to_string(),
                Value::String(resolved.to_string_lossy().into_owned()),
            );
        }
        out.insert("files".to_string(), Value::Bool(true));
    } else if is_dir {
        out.insert(
            "directory".to_string(),
            Value::String(resolved.to_string_lossy().into_owned()),
        );
    } else {
        out.insert(
            "file".to_string(),
            Value::String(resolved.to_string_lossy().into_owned()),
        );
    }

    Ok(Translated {
        command: "outline".into(),
        args: out,
    })
}

/// True when a `targets` entry carries nothing: null, or an object whose
/// path and symbol are both missing, null, or blank. Models that fill every
/// declared property send such entries (`{path: "", symbol: ""}`) when they
/// mean "no targets". Any other value is real input and is validated.
fn zoom_target_entry_is_empty(entry: &Value) -> bool {
    let blank = |value: Option<&Value>| match value {
        None | Some(Value::Null) => true,
        Some(Value::String(value)) => value.trim().is_empty(),
        Some(_) => false,
    };
    match entry {
        Value::Null => true,
        Value::Object(obj) => {
            blank(obj.get("path")) && blank(obj.get("filePath")) && blank(obj.get("symbol"))
        }
        _ => false,
    }
}

fn zoom_targets_provided(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    if is_empty_param(value) {
        return false;
    }
    match value {
        Value::Array(items) => !items.iter().all(zoom_target_entry_is_empty),
        Value::Object(_) => !zoom_target_entry_is_empty(value),
        _ => false,
    }
}

/// Translate the (already placeholder-free) `targets` entries into native
/// `{file, symbol, target_label}` objects. An entry missing its path or its
/// symbol is forwarded with that field empty rather than refused here: the
/// zoom command turns it into its own per-target error line and still answers
/// every other entry in the batch.
fn translate_zoom_targets(targets_value: &Value, project_root: &Path) -> Vec<Value> {
    let target_values: Vec<&Value> = match targets_value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![targets_value],
        _ => Vec::new(),
    };

    let mut out = Vec::with_capacity(target_values.len());
    for target in target_values {
        let field = |key: &str| {
            target
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        let file_path = field("path");
        let symbol = field("symbol");
        let mut target_out = Map::new();
        match file_path {
            Some(file_path) => {
                insert_file_or_github_target(&mut target_out, project_root, file_path);
            }
            None => {
                target_out.insert("file".to_string(), Value::String(String::new()));
            }
        }
        target_out.insert(
            "symbol".to_string(),
            Value::String(symbol.unwrap_or_default().to_string()),
        );
        target_out.insert(
            "target_label".to_string(),
            Value::String(file_path.unwrap_or("(no path)").to_string()),
        );
        out.push(Value::Object(target_out));
    }
    out
}

/// True when a zoom field carries real input: not null, and not an empty or
/// whitespace-only string, array, or object.
fn zoom_field_present(value: Option<&Value>) -> bool {
    match value {
        None => false,
        Some(Value::String(value)) => !value.trim().is_empty(),
        Some(value) => !is_empty_param(value),
    }
}

fn translate_zoom(args: Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);

    let has_targets = zoom_targets_provided(map_in.get("targets"));
    let has_file_path = zoom_field_present(map_in.get("path"));
    let has_url = zoom_field_present(map_in.get("url"));
    let has_symbols = zoom_field_present(map_in.get("symbols"));

    let mut out = Map::new();

    if has_targets {
        // `targets` together with `path`/`url` + `symbols` is one merged
        // batch, not a conflict: models that fill every declared property send
        // a real same-file request next to a placeholder `targets` entry, and
        // refusing both lost the real request. The zoom command expands the
        // top-level file and symbols into batch entries; each entry that does
        // not resolve reports its own error line.
        if has_file_path && has_url {
            return Err(invalid_request(
                "Provide exactly ONE of 'filePath' or 'url' — not both",
            ));
        }
        let targets_value = map_in
            .get("targets")
            .expect("has_targets implies a targets value exists");
        let mut targets = translate_zoom_targets(targets_value, project_root);

        let base = if has_file_path {
            map_in.get("path").and_then(Value::as_str).map(str::trim)
        } else if has_url {
            map_in.get("url").and_then(Value::as_str).map(str::trim)
        } else {
            None
        };
        if let Some(base) = base {
            if has_url {
                out.insert("file".to_string(), Value::String(base.to_string()));
            } else {
                insert_file_or_github_target(&mut out, project_root, base);
            }
            out.insert("target_label".to_string(), Value::String(base.to_string()));
            insert_zoom_symbols(&mut out, &map_in)?;
        } else if has_symbols {
            // Symbols with no file to look in: each still gets its own line
            // saying the path is missing, instead of vanishing silently.
            let names: Vec<String> = match map_in.get("symbols") {
                Some(Value::String(symbol)) => vec![symbol.trim().to_string()],
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => {
                    return Err(invalid_request(
                        "'symbols' must be a string or array of strings",
                    ))
                }
            };
            let mut orphaned = names
                .into_iter()
                .map(|name| {
                    serde_json::json!({ "file": "", "symbol": name, "target_label": "(no path)" })
                })
                .collect::<Vec<_>>();
            orphaned.append(&mut targets);
            targets = orphaned;
        }
        out.insert("targets".to_string(), Value::Array(targets));

        if let Some(context_lines) = coerce_optional_int_result(
            map_in.get("contextLines"),
            "contextLines",
            1,
            9_007_199_254_740_991,
        )? {
            out.insert(
                "context_lines".to_string(),
                Value::Number(context_lines.into()),
            );
        }

        if map_in.get("callgraph").is_some_and(coerce_boolean) {
            out.insert("callgraph".to_string(), Value::Bool(true));
        }

        return Ok(Translated {
            command: "zoom".into(),
            args: out,
        });
    }

    let file_path = map_in
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    let url = map_in
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());

    match (file_path, url) {
        (None, None) => {
            return Err(invalid_request(
                "Provide exactly one of 'filePath', 'url', or 'targets'",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(invalid_request(
                "Provide exactly ONE of 'filePath' or 'url' — not both",
            ));
        }
        _ => {}
    }

    if let Some(url) = url {
        out.insert("file".to_string(), Value::String(url.to_string()));
    } else if let Some(file_path) = file_path {
        insert_file_or_github_target(&mut out, project_root, file_path);
    }

    insert_zoom_symbols(&mut out, &map_in)?;

    if let Some(context_lines) = coerce_optional_int_result(
        map_in.get("contextLines"),
        "contextLines",
        1,
        9_007_199_254_740_991,
    )? {
        out.insert(
            "context_lines".to_string(),
            Value::Number(context_lines.into()),
        );
    }

    if map_in.get("callgraph").is_some_and(coerce_boolean) {
        out.insert("callgraph".to_string(), Value::Bool(true));
    }

    Ok(Translated {
        command: "zoom".into(),
        args: out,
    })
}

fn insert_zoom_symbols(
    out: &mut Map<String, Value>,
    map_in: &Map<String, Value>,
) -> Result<(), TranslateError> {
    let Some(symbols) = map_in.get("symbols") else {
        return Ok(());
    };
    if is_empty_param(symbols) {
        return Ok(());
    }
    match symbols {
        Value::String(symbol) => {
            out.insert("symbol".to_string(), Value::String(symbol.to_string()));
        }
        Value::Array(items) => {
            // Pass the array THROUGH to the leaf (handle_zoom's
            // parse_zoom_symbol_names handles a `symbols` array natively,
            // one lookup per element). Joining into one space-separated
            // string would break multi-heading markdown/HTML zoom, whose
            // heading names legitimately contain spaces.
            let names: Vec<Value> = items
                .iter()
                .filter_map(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(|name| Value::String(name.to_string()))
                .collect();
            if !names.is_empty() {
                out.insert("symbols".to_string(), Value::Array(names));
            }
        }
        _ => {
            return Err(invalid_request(
                "'symbols' must be a string or array of strings",
            ))
        }
    }
    Ok(())
}

fn translate_conflicts(args: Value) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let mut out = Map::new();
    if let Some(path_val) = map_in.get("path") {
        if !is_empty_param(path_val) {
            if let Some(path_str) = path_val.as_str() {
                out.insert("path".to_string(), Value::String(path_str.to_string()));
            }
        }
    }

    Ok(Translated {
        command: "git_conflicts".into(),
        args: out,
    })
}

fn translate_inspect(args: Value, project_root: &Path) -> Result<Translated, TranslateError> {
    let map_in = agent_args_map(args);
    let mut out = Map::new();

    if let Some(sections) = map_in.get("sections") {
        let mut sections = sections.clone();
        parse_stringified_collection(
            &mut sections,
            "sections",
            "a string or array of strings",
            true,
        )?;
        if !is_empty_param(&sections) {
            if !sections.is_string()
                && !sections
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string))
            {
                return Err(invalid_request(
                    "sections must be a string or array of strings (got an invalid type)",
                ));
            }
            out.insert("sections".to_string(), sections);
        }
    }

    if let Some(scope) = map_in.get("scope") {
        if !is_empty_param(scope) {
            match scope {
                Value::String(s) if !s.is_empty() => {
                    let resolved = resolve_path_from_project_root(project_root, s);
                    out.insert(
                        "scope".to_string(),
                        Value::String(resolved.to_string_lossy().into_owned()),
                    );
                }
                Value::Array(arr) => {
                    let resolved: Vec<Value> = arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|entry| {
                            let p = resolve_path_from_project_root(project_root, entry);
                            Value::String(p.to_string_lossy().into_owned())
                        })
                        .collect();
                    out.insert("scope".to_string(), Value::Array(resolved));
                }
                other => {
                    out.insert("scope".to_string(), other.clone());
                }
            }
        }
    }

    if let Some(top_k) = coerce_optional_int_result(map_in.get("topK"), "topK", 1, 100)? {
        out.insert("topK".to_string(), Value::Number(top_k.into()));
    }

    Ok(Translated {
        command: "inspect".into(),
        args: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_aliases_normalize_equal_and_reject_conflicts() {
        let project = Path::new("/project");
        let legacy = serde_json::json!({"filePath": "src/main.ts", "content": "x"});
        let canonical = serde_json::json!({"path": "src/main.ts", "content": "x"});
        assert_eq!(
            subc_translate_owned("write", legacy, project).expect("legacy path"),
            subc_translate_owned("write", canonical, project).expect("canonical path")
        );

        let conflict = serde_json::json!({"path": "src/a.ts", "filePath": "src/b.ts"});
        let error = subc_translate_owned("read", conflict, project).expect_err("conflict");
        assert_eq!(error.code, "invalid_request");
        assert!(error.message.contains("path"));
        assert!(error.message.contains("filePath"));
    }

    #[test]
    fn path_aliases_keep_unicode_scalar_equality_strict() {
        let project = Path::new("/project");
        let equal = serde_json::json!({"path": "src/😀.ts", "filePath": "src/😀.ts"});
        assert!(subc_translate_owned("read", equal, project).is_ok());

        let canonically_different = serde_json::json!({
            "path": "src/é.ts",
            "filePath": "src/e\u{301}.ts"
        });
        let error = subc_translate_owned("read", canonically_different, project)
            .expect_err("different Unicode normalization");
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn inspect_parses_stringified_sections_and_preserves_scalar_sections() {
        let project = Path::new("/project");
        for (input, expected) in [
            (
                serde_json::json!("[\"dead_code\"]"),
                serde_json::json!(["dead_code"]),
            ),
            (
                serde_json::json!("[\"todos\",\"dead_code\"]"),
                serde_json::json!(["todos", "dead_code"]),
            ),
            (serde_json::json!("all"), serde_json::json!("all")),
        ] {
            let result =
                subc_translate_owned("inspect", serde_json::json!({"sections":input}), project)
                    .expect("inspect sections");
            assert_eq!(result.args["sections"], expected);
        }
        let error = subc_translate_owned(
            "inspect",
            serde_json::json!({"sections":"[\"dead_code\""}),
            project,
        )
        .expect_err("invalid JSON sections");
        assert!(
            error.message.contains("sections") && error.message.contains("valid JSON"),
            "{error:?}"
        );
    }

    #[test]
    fn zoom_parses_stringified_targets_and_rejects_wrong_types() {
        let project = Path::new("/project");
        for entries in [
            serde_json::json!({ "path": "src/a.ts", "symbol": "foo" }),
            serde_json::json!([{ "path": "src/a.ts", "symbol": "foo" }]),
            serde_json::json!([
                { "path": "src/a.ts", "symbol": "foo" },
                { "path": "src/b.ts", "symbol": "bar" }
            ]),
        ] {
            let parsed =
                subc_translate_owned("zoom", serde_json::json!({ "targets": entries }), project)
                    .expect("array targets");
            let stringified = subc_translate_owned(
                "zoom",
                serde_json::json!({ "targets": entries.to_string() }),
                project,
            )
            .expect("stringified targets");
            assert_eq!(stringified, parsed);
        }
        for value in [
            serde_json::json!("[{bad"),
            serde_json::json!("not JSON"),
            serde_json::json!(42),
        ] {
            let error =
                subc_translate_owned("zoom", serde_json::json!({ "targets": value }), project)
                    .expect_err("invalid targets");
            assert!(
                error
                    .message
                    .contains("targets must be an array of {path, symbol}"),
                "{error:?}"
            );
        }
    }

    #[test]
    fn github_resource_targets_pass_through_read_outline_and_zoom_unresolved() {
        // The command handlers route `issue://` and `pr://` themselves; a
        // translate step that resolved them against the project root produced
        // `/project/issue:/310` and a "file not found" from every tool but
        // `read`. The scheme must survive translation in all three tools and
        // in zoom's batched targets.
        let project = Path::new("/project");
        for tool in ["read", "outline"] {
            let args = if tool == "read" {
                serde_json::json!({ "path": "issue://310" })
            } else {
                serde_json::json!({ "target": "pr://302" })
            };
            let translated = subc_translate_owned(tool, args, project).expect(tool);
            let file = translated.args["file"].as_str().expect("file");
            assert!(
                file.starts_with("issue://") || file.starts_with("pr://"),
                "{tool}: GitHub target was resolved as a path: {file}"
            );
        }
        let zoom = subc_translate_owned(
            "zoom",
            serde_json::json!({ "path": "pr://302", "symbols": "4" }),
            project,
        )
        .expect("zoom path form");
        assert_eq!(zoom.args["file"].as_str(), Some("pr://302"));
        let batched = subc_translate_owned(
            "zoom",
            serde_json::json!({ "targets": [{ "path": "issue://310", "symbol": "1" }] }),
            project,
        )
        .expect("zoom targets form");
        assert_eq!(
            batched.args["targets"][0]["file"].as_str(),
            Some("issue://310")
        );
        // A plain relative path still resolves against the project root.
        let plain = subc_translate_owned(
            "outline",
            serde_json::json!({ "target": "src/lib.rs" }),
            project,
        )
        .expect("plain outline");
        // The resolver lexically normalizes the joined path, so on Windows the
        // POSIX-spelled test root comes back with backslashes throughout; compare
        // the normalized form, not a hand-joined one.
        let expected = resolve_path_from_project_root(project, "src/lib.rs");
        assert_eq!(
            plain.args["file"].as_str(),
            Some(expected.to_string_lossy().as_ref()),
            "a plain relative target must still resolve against the project root"
        );
    }

    #[test]
    fn github_addresses_reach_write_and_edit_handlers_unresolved() {
        // Resolving `issue://369` against the root produced `/project/issue:/369`,
        // which the write handler no longer recognised as a GitHub address and
        // created as a local file.
        let project = Path::new("/project");
        let write = subc_translate_owned(
            "write",
            serde_json::json!({ "path": "issue://369", "content": "body" }),
            project,
        )
        .expect("write");
        assert_eq!(write.command, "write");
        assert_eq!(write.args["file"].as_str(), Some("issue://369"));

        let edit = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "pr://owner/repo/9/comments/2",
                "edits": [{ "oldString": "a", "newString": "b" }],
            }),
            project,
        )
        .expect("single find/replace edit");
        assert_eq!(edit.command, "edit_match");
        assert_eq!(
            edit.args["file"].as_str(),
            Some("pr://owner/repo/9/comments/2")
        );
        assert_eq!(edit.args["match"].as_str(), Some("a"));
        assert_eq!(edit.args["replacement"].as_str(), Some("b"));

        for (label, args) in [
            (
                "append",
                serde_json::json!({ "path": "issue://7", "appendContent": "x" }),
            ),
            (
                "symbol",
                serde_json::json!({ "path": "issue://7", "symbol": "f", "content": "x" }),
            ),
            (
                "top-level find/replace",
                serde_json::json!({ "path": "issue://7/comments/1", "oldString": "a", "newString": "b" }),
            ),
        ] {
            let translated = subc_translate_owned("edit", args, project).expect(label);
            assert_eq!(
                translated.args["file"]
                    .as_str()
                    .map(|file| file.starts_with("issue://")),
                Some(true),
                "{label}: GitHub address was resolved as a path"
            );
            assert_ne!(translated.command, "batch", "{label}");
        }

        for (label, args) in [
            (
                "line range",
                serde_json::json!({
                    "path": "issue://7",
                    "edits": [{ "startLine": 1, "endLine": 1, "content": "x" }],
                }),
            ),
            (
                "two entries",
                serde_json::json!({
                    "path": "issue://7/comments/1",
                    "edits": [
                        { "oldString": "a", "newString": "b" },
                        { "oldString": "c", "newString": "d" },
                    ],
                }),
            ),
        ] {
            let error = subc_translate_owned("edit", args, project).expect_err(label);
            assert_eq!(error.code, "invalid_request", "{label}");
            assert!(
                error.message.contains("GitHub"),
                "{label}: {}",
                error.message
            );
        }
    }

    #[test]
    fn file_only_mutation_tools_refuse_github_addresses() {
        let project = Path::new("/project");
        for (tool, args) in [
            (
                "delete",
                serde_json::json!({ "files": ["src/a.rs", "issue://7"] }),
            ),
            (
                "move",
                serde_json::json!({ "path": "src/a.rs", "destination": "pr://3" }),
            ),
            (
                "move",
                serde_json::json!({ "path": "issue://7", "destination": "src/a.rs" }),
            ),
            (
                "import",
                serde_json::json!({ "op": "organize", "path": "issue://7" }),
            ),
            (
                "safety",
                serde_json::json!({ "op": "checkpoint", "name": "c", "files": ["issue://7"] }),
            ),
            (
                "safety",
                serde_json::json!({ "op": "undo", "path": "pr://3" }),
            ),
            (
                "ast_replace",
                serde_json::json!({
                    "pattern": "a",
                    "rewrite": "b",
                    "lang": "rust",
                    "paths": ["issue://7"],
                }),
            ),
        ] {
            let error = subc_translate_owned(tool, args.clone(), project)
                .expect_err(&format!("{tool} {args}"));
            assert_eq!(error.code, "github_resource_not_file", "{tool} {args}");
            assert!(
                error.message.contains("github.write"),
                "{tool}: refusal must say how GitHub writes work: {}",
                error.message
            );
        }
    }

    #[test]
    fn empty_or_null_optional_path_is_stripped_as_absent() {
        let project = Path::new("/project");
        // Optional-path tools: an empty-string or null `path` is treated as
        // not supplied, so the translated args carry no `path` key at all.
        // Each tool still needs its own required fields, so supply them and
        // assert the empty path sentinel is gone from the output.
        let conflicts =
            subc_translate_owned("conflicts", serde_json::json!({ "path": "" }), project)
                .expect("conflicts with empty path");
        assert!(!conflicts.args.contains_key("path"));

        let grep = subc_translate_owned(
            "grep",
            serde_json::json!({ "pattern": "x", "path": "" }),
            project,
        )
        .expect("grep with empty path");
        assert!(!grep.args.contains_key("path"));

        let search = subc_translate_owned(
            "search",
            serde_json::json!({ "query": "x", "path": null }),
            project,
        )
        .expect("search with null path");
        assert!(!search.args.contains_key("path"));

        let safety = subc_translate_owned(
            "safety",
            serde_json::json!({ "op": "list", "path": "" }),
            project,
        )
        .expect("safety with empty path");
        assert!(!safety.args.contains_key("path"));

        // zoom with only empty path sentinels has no target at all, which is
        // the "absent" outcome: it reports the missing-target error rather than
        // the well-formed-Unicode one.
        let zoom = subc_translate_owned(
            "zoom",
            serde_json::json!({ "path": "", "filePath": "" }),
            project,
        )
        .expect_err("zoom with only empty path sentinels");
        assert!(zoom.message.contains("Provide exactly one"));

        // callgraph's optional `toPath` alias is stripped the same way.
        let callgraph = subc_translate_owned(
            "callgraph",
            serde_json::json!({
                "path": "src/main.ts",
                "toPath": "",
                "op": "callers",
                "symbol": "main"
            }),
            project,
        )
        .expect("callgraph with empty toPath");
        assert!(!callgraph.args.contains_key("to_path"));
    }

    #[test]
    fn required_tools_report_empty_path_as_missing_not_malformed() {
        let project = Path::new("/project");
        // `edit` is excluded: its full translation runs the edit contract
        // (mode selection) before path validation, so an empty path reports
        // the no-mode error, matching the TypeScript prepareCanonicalEditArguments.
        for tool in ["read", "write", "move", "import"] {
            let error = subc_translate_owned(tool, serde_json::json!({ "path": "" }), project)
                .expect_err("empty required path");
            assert_eq!(error.code, "invalid_request");
            assert!(
                error.message.contains("'path' is required")
                    || error.message.contains("missing required param"),
                "{tool} empty path message: {}",
                error.message
            );
            assert!(
                !error.message.contains("well-formed Unicode"),
                "{tool} must not report the well-formed error for an empty path"
            );
        }
        // edit with only an empty path sentinel reports the no-mode error, not
        // the well-formed-Unicode one.
        let edit = subc_translate_owned("edit", serde_json::json!({ "path": "" }), project)
            .expect_err("edit empty path");
        assert!(edit.message.contains("exactly one of"));
        assert!(!edit.message.contains("well-formed Unicode"));
    }

    #[test]
    fn empty_optional_path_sentinel_does_not_mask_a_non_string_error() {
        let project = Path::new("/project");
        // The strip is bounded to empty/null, not "anything falsy": a
        // non-string value on an optional field still throws the well-formed
        // error. (Rust strings cannot hold a lone UTF-16 surrogate, so a number
        // is the closest non-string control.)
        for tool in ["grep", "search", "conflicts", "zoom", "safety"] {
            let error = subc_translate_owned(tool, serde_json::json!({ "path": 42 }), project)
                .expect_err("non-string optional path");
            assert_eq!(error.code, "invalid_request");
            assert!(
                error.message.contains("well-formed Unicode"),
                "{tool} non-string path message: {}",
                error.message
            );
        }
    }

    #[test]
    fn grep_translation_splits_comma_separated_exact_include_names() {
        let translated = subc_translate_owned(
            "grep",
            serde_json::json!({
                "pattern": "(fail)|error",
                "path": ".cortexkit/alfonso/reports",
                "include": "a.stderr.log,b.stderr.log,c.stderr.log"
            }),
            Path::new("/project"),
        )
        .expect("agent grep arguments should translate");

        assert_eq!(translated.command, "grep");
        assert_eq!(
            translated.args.get("include"),
            Some(&serde_json::json!([
                "**/a.stderr.log",
                "**/b.stderr.log",
                "**/c.stderr.log"
            ]))
        );
    }

    #[test]
    fn owned_write_translation_moves_content_buffer() {
        let content = "x".repeat(256 * 1024);
        let content_ptr = content.as_ptr();
        let content_len = content.len();
        let mut arguments = Map::new();
        arguments.insert(
            "filePath".to_string(),
            Value::String("src/generated.ts".to_string()),
        );
        arguments.insert("content".to_string(), Value::String(content));

        let translated =
            subc_translate_owned("write", Value::Object(arguments), Path::new("/project"))
                .expect("write translation succeeds");
        let translated_content = translated
            .args
            .get("content")
            .and_then(Value::as_str)
            .expect("translated write keeps content");

        assert_eq!(translated_content.len(), content_len);
        assert_eq!(translated_content.as_ptr(), content_ptr);
    }

    #[test]
    fn edit_normalization_orders_contract_checks_before_path_resolution() {
        let project = Path::new("/project");
        let conflict = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/main.ts",
                "appendContent": "x",
                "edits": "not-json"
            }),
            project,
        )
        .expect_err("mode conflict");
        assert_eq!(conflict.code, "invalid_request");
        assert!(conflict.message.contains("conflicting modes"));

        let line_error = subc_translate_owned(
            "edit",
            serde_json::json!({ "path": 42, "startLine": 1 }),
            project,
        )
        .expect_err("top-level line range");
        assert!(line_error.message.contains("startLine"));

        let no_mode = subc_translate_owned("edit", serde_json::json!({ "path": "x" }), project)
            .expect_err("missing mode");
        assert!(no_mode.message.contains("exactly one of"));

        let retired_fields = subc_translate_owned(
            "edit",
            serde_json::json!({ "mode": "write", "file": "src/main.ts" }),
            project,
        )
        .expect_err("retired fields are ordinary unknown keys outside OpenCode aft_edit");
        assert_eq!(
            retired_fields.message,
            "Unrecognized keys: \"file\", \"mode\""
        );
    }

    #[test]
    fn edit_normalization_uses_meaningful_mode_presence() {
        let project = Path::new("/project");
        let cases = [
            (
                "edits ignores empty mode sentinels",
                serde_json::json!({
                    "filePath": "src/example.ts",
                    "edits": [{ "oldString": "old", "newString": "new" }],
                    "appendContent": "",
                    "symbol": "",
                    "content": "",
                }),
                Some("batch"),
                None,
            ),
            (
                "append ignores empty edits",
                serde_json::json!({
                    "filePath": "src/example.ts",
                    "appendContent": "append",
                    "edits": [],
                }),
                Some("edit_match"),
                None,
            ),
            (
                "symbol deletion keeps empty content",
                serde_json::json!({
                    "filePath": "src/example.ts",
                    "symbol": "target",
                    "content": "",
                }),
                Some("edit_symbol"),
                None,
            ),
            (
                "content without a symbol is rejected",
                serde_json::json!({
                    "filePath": "src/example.ts",
                    "symbol": "",
                    "content": "replacement",
                }),
                None,
                Some("property 'symbol' must be a non-empty string"),
            ),
            (
                "two real modes conflict",
                serde_json::json!({
                    "filePath": "src/example.ts",
                    "appendContent": "append",
                    "edits": [{ "oldString": "old", "newString": "new" }],
                }),
                None,
                Some("conflicting modes"),
            ),
            (
                "all empty fields have no mode",
                serde_json::json!({
                    "filePath": "src/example.ts",
                    "appendContent": "",
                    "edits": [],
                    "symbol": "",
                    "content": "",
                    "oldString": "",
                    "newString": "",
                    "replaceAll": null,
                    "occurrence": null,
                }),
                None,
                Some("exactly one of"),
            ),
        ];

        for (label, arguments, command, expected_error) in cases {
            match (command, expected_error) {
                (Some(command), None) => {
                    let translated = subc_translate_owned("edit", arguments, project)
                        .unwrap_or_else(|error| panic!("{label}: {}", error.message));
                    assert_eq!(translated.command, command, "{label}");
                    match label {
                        "edits ignores empty mode sentinels" => {
                            assert_eq!(
                                translated.args["edits"][0]["match"],
                                Value::String("old".to_string())
                            );
                            assert_eq!(
                                translated.args["edits"][0]["replacement"],
                                Value::String("new".to_string())
                            );
                        }
                        "append ignores empty edits" => {
                            assert_eq!(
                                translated.args["append_content"],
                                Value::String("append".to_string())
                            );
                        }
                        "symbol deletion keeps empty content" => {
                            assert_eq!(translated.args["content"], Value::String(String::new()));
                        }
                        _ => unreachable!("unexpected successful edit mode case"),
                    }
                }
                (None, Some(expected_error)) => {
                    let translation_error = subc_translate_owned("edit", arguments, project)
                        .expect_err("meaningful mode case must fail");
                    assert!(
                        translation_error.message.contains(expected_error),
                        "{label}: {}",
                        translation_error.message
                    );
                }
                _ => unreachable!("case must expect exactly one outcome"),
            }
        }
    }

    #[test]
    fn edit_normalization_accepts_aliases_and_rejects_ambiguous_scalars() {
        let project = Path::new("/project");
        let stringified = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/main.ts",
                "edits": "[{\"oldString\":\"before\",\"newString\":\"after\"}]"
            }),
            project,
        )
        .expect("stringified non-empty edits array");
        assert_eq!(stringified.command, "batch");

        let normalized = subc_translate_owned(
            "edit",
            serde_json::json!({
                "filePath": "src/main.ts",
                "edits": [{ "oldText": "before", "newText": "after", "occurrence": " +01 " }]
            }),
            project,
        )
        .expect("compatibility aliases");
        let item = normalized
            .args
            .get("edits")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .expect("translated edit item");
        assert_eq!(item.get("match").and_then(Value::as_str), Some("before"));
        assert_eq!(item.get("occurrence").and_then(Value::as_u64), Some(1));

        for value in ["0", "00", "+0", "1.0", "1e0", "0x1", "-1"] {
            let error = subc_translate_owned(
                "edit",
                serde_json::json!({
                    "path": "src/main.ts",
                    "edits": [{ "oldString": "before", "occurrence": value }]
                }),
                project,
            )
            .expect_err("invalid occurrence spelling");
            assert!(error.message.contains("occurrence"));
        }
    }

    #[test]
    fn edit_strips_all_empty_sentinel_edit_items() {
        let project = Path::new("/project");

        // The exact GPT 5.6 Terra report shape: every optional field carries a
        // type-default sentinel and the real payload lives in appendContent.
        // The all-empty edits array must not claim the edits mode.
        let report = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/main.ts",
                "symbol": "",
                "content": "",
                "appendContent": "CONTENT IT APPENDS",
                "edits": [
                    { "oldString": "", "newString": "", "replaceAll": false,
                      "occurrence": 1, "startLine": 1, "endLine": 1, "content": "" }
                ]
            }),
            project,
        )
        .expect("all-empty sentinel edits must not claim the edits mode");
        assert_eq!(report.command, "edit_match");
        assert_eq!(
            report.args.get("append_content").and_then(Value::as_str),
            Some("CONTENT IT APPENDS")
        );

        // A sentinel item alongside one real match item: the real item
        // survives and the batch path is taken.
        let mixed = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/main.ts",
                "edits": [
                    { "oldString": "", "newString": "", "replaceAll": false,
                      "occurrence": 1, "startLine": 1, "endLine": 1, "content": "" },
                    { "oldString": "old", "newString": "new" }
                ]
            }),
            project,
        )
        .expect("real item must survive sentinel stripping");
        assert_eq!(mixed.command, "batch");
        let items = mixed.args.get("edits").and_then(Value::as_array).unwrap();
        assert_eq!(items.len(), 1, "only the real item survives");
        assert_eq!(items[0].get("match").and_then(Value::as_str), Some("old"));
        assert_eq!(
            items[0].get("replacement").and_then(Value::as_str),
            Some("new")
        );

        // A pure line-range delete item has no oldString key, so it is never a
        // sentinel even with empty content (deleting lines is real intent).
        let line_delete = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/main.ts",
                "edits": [{ "startLine": 1, "endLine": 1, "content": "" }]
            }),
            project,
        )
        .expect("pure line-range delete must stay an edits claim");
        assert_eq!(line_delete.command, "batch");

        // An empty match with a real replacement is invalid, not an omitted
        // serializer default. Report the field rather than discard the edit.
        let empty_match = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/main.ts",
                "edits": [{ "oldString": "", "newString": "x" }]
            }),
            project,
        )
        .expect_err("empty oldString must report an invalid edit, not a missing mode");
        assert_eq!(empty_match.message, "edit: edits[0].oldString is empty");

        // Stringified kitchen-sink edits + appendContent: appendContent wins.
        let stringified = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/main.ts",
                "appendContent": "APPEND",
                "edits": "[{\"oldString\":\"\",\"newString\":\"\",\"replaceAll\":false,\"occurrence\":1,\"startLine\":1,\"endLine\":1,\"content\":\"\"}]"
            }),
            project,
        )
        .expect("stringified all-empty sentinel edits must not claim edits mode");
        assert_eq!(stringified.command, "edit_match");
        assert_eq!(
            stringified
                .args
                .get("append_content")
                .and_then(Value::as_str),
            Some("APPEND")
        );
    }

    #[test]
    fn meaningful_find_payload_predicate_rejects_empty_match_mutation_control() {
        let meaningful = serde_json::json!({ "oldString": "before" });
        let empty = serde_json::json!({ "oldString": "" });
        let absent = serde_json::json!({});

        assert!(has_meaningful_find_payload(
            meaningful.as_object().expect("meaningful item object")
        ));
        assert!(!has_meaningful_find_payload(
            empty.as_object().expect("empty item object")
        ));
        assert!(!has_meaningful_find_payload(
            absent.as_object().expect("absent item object")
        ));
    }

    #[test]
    fn edit_normalization_applies_symmetric_item_sentinel_precedence() {
        let project = Path::new("/project");

        // A meaningful match wins when range fields are blank serializer
        // defaults, including all three range-shaped fields.
        for item in [
            serde_json::json!({
                "oldString": "before", "newString": "after",
                "startLine": 1, "endLine": 1, "content": "",
            }),
            serde_json::json!({
                "oldString": "before", "newString": "after",
                "startLine": 1, "endLine": 1,
            }),
        ] {
            let translated = subc_translate_owned(
                "edit",
                serde_json::json!({ "path": "src/example.ts", "edits": [item] }),
                project,
            )
            .expect("empty or absent range payload must yield to find/replace");
            assert_eq!(
                translated.args["edits"],
                serde_json::json!([{ "match": "before", "replacement": "after" }]),
            );
        }

        // A range delete with no find/replace fields is real intent, not a
        // serializer sentinel.
        let line_delete = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/example.ts",
                "edits": [{ "startLine": 1, "endLine": 1, "content": "" }],
            }),
            project,
        )
        .expect("bare line-range delete must remain a range edit");
        assert_eq!(
            line_delete.args["edits"],
            serde_json::json!([{ "line_start": 1, "line_end": 1, "content": "" }]),
        );

        // An all-default object carries no edit payload, so it is removed
        // before validation and leaves no selected edit mode.
        let all_sentinels = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/example.ts",
                "edits": [{
                    "oldString": "", "newString": "", "replaceAll": false,
                    "occurrence": 1, "startLine": 1, "endLine": 1, "content": "",
                }],
            }),
            project,
        )
        .expect_err("all-default item must be dropped");
        assert!(all_sentinels.message.contains("exactly one of"));

        let issue_payload = serde_json::json!({
            "path": "src/example.ts",
            "edits": [{
                "content": "const value = new;",
                "startLine": 14,
                "endLine": 14,
                "oldString": "",
                "newString": "",
                "replaceAll": false,
                "occurrence": 1,
            }],
        });
        let translated = subc_translate_owned("edit", issue_payload, project)
            .expect("line-range sentinels must not select find/replace mode");
        assert_eq!(translated.command, "batch");
        assert_eq!(
            translated.args["edits"],
            serde_json::json!([{
                "content": "const value = new;",
                "line_start": 14,
                "line_end": 14,
            }]),
        );

        for arguments in [
            serde_json::json!({
                "path": "src/example.ts",
                "edits": [{
                    "content": "replacement", "startLine": 14, "endLine": 14,
                    "oldString": "meaningful", "newString": "",
                }],
            }),
            serde_json::json!({
                "path": "src/example.ts",
                "edits": [{
                    "content": "replacement", "startLine": 14, "endLine": 14,
                    "oldString": "", "newString": "", "replaceAll": true,
                }],
            }),
            serde_json::json!({
                "path": "src/example.ts",
                "edits": [{
                    "content": "replacement", "startLine": 14, "endLine": 14,
                    "oldString": "", "newString": "", "occurrence": 2,
                }],
            }),
        ] {
            let error = subc_translate_owned("edit", arguments, project)
                .expect_err("meaningful find/replace fields must remain mixed-mode errors");
            assert_eq!(
                error.message,
                "edit: edits[0] mixes find/replace and line-range fields"
            );
        }

        let empty_find = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/example.ts",
                "edits": [{ "oldString": "", "newString": "replacement" }],
            }),
            project,
        )
        .expect_err("empty find match must report the invalid field");
        assert_eq!(empty_find.message, "edit: edits[0].oldString is empty");
    }

    #[test]
    fn edit_null_sentinels_are_absent_at_both_edit_boundaries() {
        let project = Path::new("/project");

        // Null optional range fields must not turn an otherwise valid
        // find/replace item into a mixed-family request.
        let issue_payload = serde_json::json!({
            "path": "src/example.ts",
            "edits": [{
                "oldString": "gamma line three",
                "newString": "GAMMA line three",
                "replaceAll": false,
                "occurrence": null,
                "startLine": null,
                "endLine": null,
                "content": null,
            }],
        });
        let translated = subc_translate_owned("edit", issue_payload, project)
            .expect("null range sentinels must not create a mode conflict");
        assert_eq!(
            translated.args["edits"],
            serde_json::json!([{
                "match": "gamma line three",
                "replacement": "GAMMA line three",
            }]),
        );

        // Cover every nullable item field so a change to one stripping branch
        // cannot silently restore the false mixed-mode rejection.
        for field in [
            "newString",
            "replaceAll",
            "occurrence",
            "startLine",
            "endLine",
            "content",
        ] {
            let mut item = serde_json::json!({
                "oldString": "before",
                "newString": "after",
            });
            item.as_object_mut()
                .expect("edit item object")
                .insert(field.to_string(), Value::Null);
            let translated = subc_translate_owned(
                "edit",
                serde_json::json!({ "path": "src/example.ts", "edits": [item] }),
                project,
            )
            .unwrap_or_else(|error| panic!("null {field} sentinel: {}", error.message));
            let expected = if field == "newString" {
                serde_json::json!([{ "match": "before" }])
            } else {
                serde_json::json!([{ "match": "before", "replacement": "after" }])
            };
            assert_eq!(
                translated.args["edits"], expected,
                "null {field} must be absent",
            );
        }

        // A pure line-range delete remains real intent even when the host
        // emits nulls for all of the unrelated find/replace fields.
        let line_delete = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/example.ts",
                "edits": [{
                    "startLine": 1,
                    "endLine": 1,
                    "content": "",
                    "oldString": null,
                    "newString": null,
                    "replaceAll": null,
                    "occurrence": null,
                }],
            }),
            project,
        )
        .expect("null find fields must not hide a line-range delete");
        assert_eq!(
            line_delete.args["edits"],
            serde_json::json!([{
                "content": "",
                "line_start": 1,
                "line_end": 1,
            }]),
        );

        // An item containing only null sentinels has no edit intent and is
        // dropped, while a non-null malformed match still reports its own
        // missing required field.
        let null_item = serde_json::json!({
            "oldString": null,
            "newString": null,
            "replaceAll": null,
            "occurrence": null,
            "startLine": null,
            "endLine": null,
            "content": null,
        });
        let no_mode = subc_translate_owned(
            "edit",
            serde_json::json!({ "path": "src/example.ts", "edits": [null_item] }),
            project,
        )
        .expect_err("all-null edit item must be dropped as a sentinel");
        assert!(no_mode.message.contains("exactly one of"));

        let mixed = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/example.ts",
                "edits": [
                    {
                        "oldString": null,
                        "newString": null,
                        "replaceAll": null,
                        "occurrence": null,
                        "startLine": null,
                        "endLine": null,
                        "content": null,
                    },
                    { "oldString": "before", "newString": "after" },
                ],
            }),
            project,
        )
        .expect("real edit must survive an all-null sentinel");
        assert_eq!(
            mixed.args["edits"],
            serde_json::json!([{ "match": "before", "replacement": "after" }]),
        );

        let malformed = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/example.ts",
                "edits": [{ "oldString": null, "newString": "replacement" }],
            }),
            project,
        )
        .expect_err("null oldString with real replacement must remain invalid");
        assert!(malformed.message.contains("oldString is null"));

        // When `edits[]` is present, null values for unrelated top-level mode
        // fields are treated as omitted so the batch remains valid.
        let top_level_base = serde_json::json!({
            "path": "src/example.ts",
            "edits": [{ "oldString": "before", "newString": "after" }],
        });
        for field in [
            "appendContent",
            "symbol",
            "content",
            "oldString",
            "newString",
            "replaceAll",
            "occurrence",
        ] {
            let mut arguments = top_level_base.clone();
            arguments
                .as_object_mut()
                .expect("edit arguments object")
                .insert(field.to_string(), Value::Null);
            subc_translate_owned("edit", arguments, project)
                .unwrap_or_else(|error| panic!("top-level null {field}: {}", error.message));
        }
        let null_edits = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/example.ts",
                "appendContent": "append",
                "edits": null,
            }),
            project,
        )
        .expect("top-level null edits must be absent");
        assert_eq!(null_edits.command, "edit_match");

        let occurrence_zero = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/example.ts",
                "edits": [{ "oldString": "before", "occurrence": 0 }],
            }),
            project,
        )
        .expect_err("occurrence zero must not be treated as a null sentinel");
        assert!(occurrence_zero.message.contains("occurrence"));
    }

    #[test]
    fn incomplete_symbol_mode_errors_distinguish_missing_and_null_properties() {
        let project = Path::new("/project");
        let cases = [
            (
                "missing content",
                serde_json::json!({ "path": "src/main.ts", "symbol": "greetUser" }),
                "edit: incomplete symbol mode: missing property 'content'. Retry with `symbol` + `content`, or use `edits[]`.",
            ),
            (
                "missing symbol",
                serde_json::json!({ "path": "src/main.ts", "content": "fn greet_user() {}" }),
                "edit: incomplete symbol mode: missing property 'symbol'. Retry with `symbol` + `content`, or use `edits[]`.",
            ),
            (
                "null content",
                serde_json::json!({
                    "path": "src/main.ts",
                    "symbol": "greetUser",
                    "content": null,
                }),
                "edit: incomplete symbol mode: property 'content' is null. Retry with `symbol` + `content`, or use `edits[]`.",
            ),
            (
                "null symbol",
                serde_json::json!({
                    "path": "src/main.ts",
                    "symbol": null,
                    "content": "fn greet_user() {}",
                }),
                "edit: incomplete symbol mode: property 'symbol' is null. Retry with `symbol` + `content`, or use `edits[]`.",
            ),
        ];

        let mut messages = Vec::new();
        for (case, arguments, expected) in cases {
            let error = subc_translate_owned("edit", arguments, project).expect_err(case);
            messages.push(error.message.clone());
            assert_eq!(error.message, expected, "{case}");
        }
        messages.sort();
        messages.dedup();
        assert_eq!(
            messages.len(),
            4,
            "each incomplete shape must be distinguishable"
        );

        let symbol = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/main.ts",
                "symbol": "greetUser",
                "content": "fn greet_user() {}",
            }),
            project,
        )
        .expect("complete symbol mode");
        assert_eq!(symbol.command, "edit_symbol");

        let edits = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/main.ts",
                "edits": [{ "oldString": "before", "newString": "after" }],
            }),
            project,
        )
        .expect("complete edits mode");
        assert_eq!(edits.command, "batch");
    }

    #[test]
    fn edit_mode_errors_steer_away_from_empty_sentinels() {
        let project = Path::new("/project");
        let steering = "Omit unused optional fields entirely; do not send empty strings or empty arrays for them.";

        let conflict = subc_translate_owned(
            "edit",
            serde_json::json!({
                "path": "src/main.ts",
                "appendContent": "x",
                "edits": [{ "oldString": "old", "newString": "new" }]
            }),
            project,
        )
        .expect_err("conflicting modes");
        assert!(conflict.message.contains("conflicting modes"));
        assert!(
            conflict.message.contains(steering),
            "conflicting-modes error must steer: {}",
            conflict.message
        );

        let no_mode = subc_translate_owned(
            "edit",
            serde_json::json!({ "path": "src/main.ts" }),
            project,
        )
        .expect_err("no mode");
        assert!(no_mode.message.contains("exactly one of"));
        assert!(
            no_mode.message.contains(steering),
            "no-mode error must steer: {}",
            no_mode.message
        );
    }

    #[test]
    fn search_legacy_hint_is_accepted_and_ignored() {
        let translated = subc_translate_owned(
            "search",
            serde_json::json!({
                "query": "outside <touser>",
                "topK": 5,
                "hint": "literal"
            }),
            Path::new("/project"),
        )
        .expect("legacy search hint must not reject the request");

        assert_eq!(translated.command, "semantic_search");
        assert_eq!(
            translated.args.get("query").and_then(Value::as_str),
            Some("outside <touser>")
        );
        assert_eq!(
            translated.args.get("top_k").and_then(Value::as_u64),
            Some(5)
        );
        assert!(translated.args.get("hint").is_none());
    }

    #[test]
    fn search_pattern_is_forwarded_beside_or_instead_of_query() {
        use serde_json::json;
        let translate = |args: Value| subc_translate_owned("search", args, Path::new("/project"));

        let split =
            translate(json!({"query": "how is config loaded", "pattern": "load_config|Config "}))
                .expect("query and pattern");
        assert_eq!(split.command, "semantic_search");
        assert_eq!(
            split.args.get("query").and_then(Value::as_str),
            Some("how is config loaded")
        );
        // The regex is forwarded as written, trailing space included.
        assert_eq!(
            split.args.get("pattern").and_then(Value::as_str),
            Some("load_config|Config ")
        );

        let alone = translate(json!({"pattern": "^export"})).expect("pattern alone");
        assert!(alone.args.get("query").is_none());
        assert_eq!(
            alone.args.get("pattern").and_then(Value::as_str),
            Some("^export")
        );

        // A blank pattern is absent: the request is the query-only request.
        let blank = translate(json!({"query": "needle", "pattern": "   "})).expect("blank");
        assert_eq!(
            blank.args,
            translate(json!({"query": "needle"}))
                .expect("query only")
                .args
        );

        // An invalid regex is not judged here: it reaches the engine, which
        // refuses it with grep's `invalid_pattern` code and text.
        let invalid = translate(json!({"pattern": "["})).expect("forwarded");
        assert_eq!(
            invalid.args.get("pattern").and_then(Value::as_str),
            Some("[")
        );

        for args in [
            json!({}),
            json!({"pattern": " "}),
            json!({"query": "", "pattern": null}),
        ] {
            let error = translate(args.clone()).expect_err("no usable input");
            assert_eq!(error.code, "invalid_request", "{args}");
        }
        let error = translate(json!({"query": "x", "pattern": 3})).expect_err("non-string");
        assert!(error.message.contains("pattern"));
    }

    #[test]
    fn search_offset_is_forwarded_and_bounded() {
        let translated = subc_translate_owned(
            "search",
            serde_json::json!({"query": "ranked results", "offset": 42}),
            Path::new("/project"),
        )
        .expect("valid search offset");
        assert_eq!(
            translated.args.get("offset").and_then(Value::as_u64),
            Some(42)
        );

        for offset in [serde_json::json!(-1), serde_json::json!(100_001)] {
            let error = subc_translate_owned(
                "search",
                serde_json::json!({"query": "ranked results", "offset": offset}),
                Path::new("/project"),
            )
            .expect_err("out-of-range search offset must be rejected");
            assert_eq!(error.code, "invalid_request");
            assert!(error.message.contains("offset"));
        }
    }

    #[test]
    fn search_top_k_outside_public_bounds_is_invalid_request() {
        for top_k in [serde_json::json!(0), serde_json::json!(51)] {
            let error = subc_translate_owned(
                "search",
                serde_json::json!({"query": "ranked results", "topK": top_k}),
                Path::new("/project"),
            )
            .expect_err("out-of-range search topK must be rejected");
            assert_eq!(error.code, "invalid_request");
            assert!(error.message.contains("topK"));
        }
    }

    // supports_tool() gates whether run_tool_call translates or passes a name
    // through as a native command. If a translate arm is added but the
    // allowlist isn't updated, that tool would silently bypass translation and
    // dispatch as a raw native command — this proves the two sets agree.
    #[test]
    fn powershell_translation_selects_the_unified_bash_executor() {
        let translated = subc_translate_owned(
            "powershell",
            serde_json::json!({ "command": "Get-ChildItem", "workdir": "scripts" }),
            Path::new("/project"),
        )
        .expect("PowerShell tool must translate");

        assert_eq!(translated.command, "bash");
        assert_eq!(
            translated.args.get("shell"),
            Some(&Value::String("powershell".into()))
        );
        // Compare as paths, not strings: the product re-joins every component
        // with the native separator (Windows renders \project\scripts), so no
        // single expected literal is right on both platforms. Path equality is
        // component-based and separator-agnostic.
        let workdir = translated
            .args
            .get("workdir")
            .and_then(Value::as_str)
            .expect("workdir must translate");
        assert_eq!(
            Path::new(workdir),
            Path::new("/project").join("scripts").as_path()
        );
    }

    #[test]
    fn supports_tool_covers_every_translated_arm() {
        for name in [
            "bash",
            "powershell",
            "status",
            "read",
            "write",
            "edit",
            "apply_patch",
            "grep",
            "glob",
            "search",
            "outline",
            "zoom",
            "inspect",
            "callgraph",
            "conflicts",
            "ast_search",
            "ast_replace",
            "delete",
            "move",
            "import",
            "safety",
            "bash_status",
            "bash_kill",
            "bash_write",
        ] {
            // Every name the allowlist claims support for must actually
            // translate (not return unsupported_tool). A no-arg call may fail
            // validation, but it must never be unsupported_tool.
            let err =
                subc_translate_owned(name, Value::Object(Map::new()), Path::new("/project")).err();
            assert_ne!(
                err.as_ref().map(|e| e.code),
                Some("unsupported_tool"),
                "{name} is in supports_tool but has no translate arm"
            );
            assert!(
                supports_tool(name),
                "{name} translates but is missing from supports_tool"
            );
        }
        // A name that is not a tool must be rejected by both.
        assert!(!supports_tool("definitely_not_a_tool"));
    }

    #[test]
    fn bash_companion_translation_maps_catalog_keys_and_keeps_native_ones() {
        // Catalog spelling from a model.
        let translated = subc_translate_owned(
            "bash_status",
            serde_json::json!({ "taskId": "bash-1", "outputMode": "screen" }),
            Path::new("/project"),
        )
        .expect("bash_status must translate");
        assert_eq!(translated.command, "bash_status");
        assert_eq!(
            Value::Object(translated.args),
            serde_json::json!({ "task_id": "bash-1", "output_mode": "screen" })
        );

        // snake_case arguments as AFT's own plugins send them when polling
        // with output offsets.
        let native = serde_json::json!({
            "task_id": "bash-2",
            "output_mode": "raw",
            "output_offset": 10,
            "stderr_offset": 4,
        });
        let translated = subc_translate_owned("bash_status", native.clone(), Path::new("/project"))
            .expect("native bash_status must translate");
        assert_eq!(Value::Object(translated.args), native);

        let translated = subc_translate_owned(
            "bash_write",
            serde_json::json!({ "taskId": "bash-3", "input": ["q", { "key": "enter" }] }),
            Path::new("/project"),
        )
        .expect("bash_write must translate");
        assert_eq!(translated.command, "bash_write");
        assert_eq!(
            Value::Object(translated.args),
            serde_json::json!({ "task_id": "bash-3", "input": ["q", { "key": "enter" }] })
        );

        let translated = subc_translate_owned(
            "bash_kill",
            serde_json::json!({ "taskId": "bash-4" }),
            Path::new("/project"),
        )
        .expect("bash_kill must translate");
        assert_eq!(translated.command, "bash_kill");
        assert_eq!(
            Value::Object(translated.args),
            serde_json::json!({ "task_id": "bash-4" })
        );
    }
}

#[cfg(test)]
mod model_boolean_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_boolean_tool_arguments_accept_trimmed_model_strings() {
        let root = Path::new("/project");
        let cases = [
            ("bash", json!({"command": "true"}), "wait"),
            ("bash", json!({"command": "true"}), "background"),
            ("bash", json!({"command": "true"}), "compressed"),
            ("bash", json!({"command": "true"}), "pty"),
            ("bash", json!({"command": "true"}), "foreground_orchestrate"),
            ("bash", json!({"command": "true"}), "block_to_completion"),
            ("bash", json!({"command": "true"}), "permissions_requested"),
            ("read", json!({"path": "file.ts"}), "vision_capability"),
            ("search", json!({"query": "needle"}), "includeTests"),
            ("outline", json!({"target": "src"}), "files"),
            ("outline", json!({"target": "src"}), "includeTests"),
            ("outline", json!({"target": "src"}), "include_tests"),
            (
                "zoom",
                json!({"path": "file.ts", "symbols": "main"}),
                "callgraph",
            ),
            (
                "callgraph",
                json!({"path": "file.ts", "symbol": "main", "op": "callers"}),
                "includeTests",
            ),
            (
                "ast_replace",
                json!({"pattern": "foo()", "rewrite": "bar()", "lang": "typescript"}),
                "dryRun",
            ),
            (
                "ast_replace",
                json!({"pattern": "foo()", "rewrite": "bar()", "lang": "typescript"}),
                "dry_run",
            ),
            ("delete", json!({"files": ["old"]}), "recursive"),
            (
                "import",
                json!({"path": "file.ts", "op": "add", "module": "pkg"}),
                "typeOnly",
            ),
            (
                "edit",
                json!({"path": "file.ts", "oldString": "old", "newString": "new"}),
                "replaceAll",
            ),
        ];
        for (tool, args, field) in cases {
            for (raw, expected) in [
                (" TrUe ", true),
                (" 1 ", true),
                (" FaLsE ", false),
                (" 0 ", false),
            ] {
                let mut typed = args.clone();
                typed[field] = json!(expected);
                let mut shaped = args.clone();
                shaped[field] = json!(raw);
                assert_eq!(
                    subc_translate(tool, &shaped, root),
                    subc_translate(tool, &typed, root),
                    "{tool}.{field}={raw}"
                );
                assert!(subc_translate(tool, &typed, root).is_ok(), "{tool}.{field}");
                if tool == "bash" {
                    assert_eq!(
                        subc_translate("powershell", &shaped, root),
                        subc_translate("powershell", &typed, root)
                    );
                }
            }
        }
        let nested = json!({"path": "file.ts", "edits": [{"oldString": "old", "newString": "new", "replaceAll": " TrUe "}]});
        let result = subc_translate("edit", &nested, root).unwrap();
        assert_eq!(result.args["edits"][0]["replaceAll"], true);
    }
}

/// Hosts whose model fills every declared tool property on every call send
/// placeholder values (`0`, `""`, `null`, `{path: "x", symbol: "x"}`) for the
/// properties they do not mean. These tests replay the shapes recorded from
/// such a model and pin that the real part of the request still works.
#[cfg(test)]
mod placeholder_argument_tests {
    use super::*;
    use serde_json::json;

    fn abs(path: &str) -> String {
        resolve_path_from_project_root(Path::new("/project"), path)
            .to_string_lossy()
            .into_owned()
    }

    fn read(args: Value) -> Result<Translated, TranslateError> {
        subc_translate_owned("read", args, Path::new("/project"))
    }

    #[test]
    fn read_refuses_the_recorded_inverted_range_by_name() {
        let error = read(json!({
            "filePath": "src/conversion-lane.ts",
            "startLine": 245,
            "endLine": 145,
            "offset": 245,
            "limit": 70
        }))
        .expect_err("an inverted range must not translate into an empty read");
        assert_eq!(error.code, "invalid_range");
        assert!(
            error.message.contains("startLine 245 is after endLine 145"),
            "{error:?}"
        );
    }

    #[test]
    fn read_zero_empty_and_null_line_fields_count_as_absent() {
        let translated = read(json!({
            "path": "a.ts",
            "startLine": 0,
            "endLine": "",
            "offset": null,
            "limit": 0
        }))
        .expect("placeholders only");
        assert!(!translated.args.contains_key("start_line"));
        assert!(!translated.args.contains_key("end_line"));
        assert!(!translated.args.contains_key("limit"));

        // Real offset/limit next to placeholder line fields keep working.
        let translated = read(json!({
            "path": "a.ts",
            "startLine": 0,
            "endLine": 0,
            "offset": 40,
            "limit": 10
        }))
        .expect("offset/limit with placeholder range");
        assert_eq!(translated.args["start_line"], 40);
        assert_eq!(translated.args["end_line"], 49);

        // Real range next to placeholder offset/limit keeps working.
        let translated = read(json!({
            "path": "a.ts",
            "startLine": 5,
            "endLine": 9,
            "offset": 0,
            "limit": 0
        }))
        .expect("range with placeholder offset/limit");
        assert_eq!(translated.args["start_line"], 5);
        assert_eq!(translated.args["end_line"], 9);
        assert!(!translated.args.contains_key("limit"));
    }

    #[test]
    fn read_start_line_wins_over_offset_and_limit_bounds_an_open_range() {
        let translated = read(json!({ "path": "a.ts", "startLine": 10, "offset": 99, "limit": 5 }))
            .expect("startLine with offset and limit");
        assert_eq!(translated.args["start_line"], 10);
        assert!(!translated.args.contains_key("end_line"));
        assert_eq!(translated.args["limit"], 5);

        let error = read(json!({ "path": "a.ts", "startLine": -3 }))
            .expect_err("a negative line is invalid, not absent");
        assert!(error.message.contains("startLine"), "{error:?}");
    }

    fn zoom(args: Value) -> Result<Translated, TranslateError> {
        subc_translate_owned("zoom", args, Path::new("/project"))
    }

    #[test]
    fn zoom_placeholder_targets_count_as_absent() {
        for targets in [
            json!([]),
            json!({}),
            json!({ "path": "", "symbol": "" }),
            json!([{ "path": "  ", "symbol": "\t" }, { "filePath": "", "symbol": null }]),
            json!(null),
        ] {
            let translated = zoom(json!({
                "path": "src/a.ts",
                "symbols": ["foo"],
                "url": "",
                "targets": targets.clone()
            }))
            .unwrap_or_else(|error| panic!("targets {targets} must count as absent: {error:?}"));
            assert!(!translated.args.contains_key("targets"), "{targets}");
            assert_eq!(translated.args["file"], abs("src/a.ts"));
            assert_eq!(translated.args["symbols"], json!(["foo"]));
        }

        let error = zoom(json!({ "targets": [{ "path": "", "symbol": "" }] }))
            .expect_err("nothing usable given");
        assert!(error.message.contains("Provide exactly one"), "{error:?}");
    }

    #[test]
    fn zoom_merges_real_path_and_symbols_with_a_placeholder_target() {
        let translated = zoom(json!({
            "path": "src/a.ts",
            "symbols": ["foo", "bar"],
            "url": "",
            "targets": { "path": "x", "symbol": "x" },
            "contextLines": 0,
            "callgraph": false
        }))
        .expect("targets plus path and symbols merge into one batch");
        assert_eq!(translated.command, "zoom");
        assert_eq!(translated.args["file"], abs("src/a.ts"));
        assert_eq!(translated.args["target_label"], "src/a.ts");
        assert_eq!(translated.args["symbols"], json!(["foo", "bar"]));
        assert_eq!(
            translated.args["targets"],
            json!([{ "file": abs("x"), "symbol": "x", "target_label": "x" }])
        );
        assert!(!translated.args.contains_key("context_lines"));
        assert!(!translated.args.contains_key("callgraph"));
    }

    #[test]
    fn zoom_partial_targets_are_forwarded_for_per_target_errors() {
        let translated = zoom(json!({
            "targets": [
                { "path": "src/a.ts", "symbol": "foo" },
                { "path": "src/b.ts", "symbol": "" }
            ]
        }))
        .expect("a partial entry does not refuse the batch");
        assert_eq!(
            translated.args["targets"],
            json!([
                { "file": abs("src/a.ts"), "symbol": "foo", "target_label": "src/a.ts" },
                { "file": abs("src/b.ts"), "symbol": "", "target_label": "src/b.ts" }
            ])
        );
    }

    #[test]
    fn safety_blank_files_entries_count_as_absent() {
        let translated = subc_translate_owned(
            "safety",
            json!({ "op": "checkpoint", "name": "cp", "files": [""], "path": "" }),
            Path::new("/project"),
        )
        .expect("blank files entry is a placeholder");
        assert!(!translated.args.contains_key("files"));
    }
}

#[cfg(test)]
mod audit_regressions {
    #[test]
    fn percent_decode_unicode_literal() {
        for text in ["%aé.txt", "50%€.txt", "%", "%a", "%gg"] {
            assert_eq!(super::percent_decode(text), text);
        }
        assert_eq!(super::percent_decode("%C3%A9%20"), "é ");
    }
}
