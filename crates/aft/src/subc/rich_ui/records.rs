//! Immutable display bodies. None of these values are part of a tool reply.
#[cfg(test)]
use std::collections::BTreeMap;

use cortexkit_role_tool_display::canonical_body;
#[cfg(test)]
use cortexkit_role_tool_display::{FieldKind, RecordSchema};
use serde_json::{json, Map, Value};

#[cfg(test)]
pub(crate) fn bash_schema() -> RecordSchema {
    RecordSchema::new(
        "aft/bash/1",
        1,
        BTreeMap::from([
            ("taskId".into(), FieldKind::Id),
            ("phase".into(), FieldKind::Code),
            ("exitCode".into(), FieldKind::Integer),
            ("outputBytes".into(), FieldKind::Integer),
            ("endReason".into(), FieldKind::Code),
            ("runDurationMs".into(), FieldKind::Integer),
            ("execution".into(), FieldKind::Code),
            ("target".into(), FieldKind::Code),
        ]),
    )
}

#[cfg(test)]
pub(crate) fn files_schema() -> RecordSchema {
    let mut fields = BTreeMap::new();
    for name in [
        "fileCount",
        "modifiedCount",
        "createdCount",
        "deletedCount",
        "movedCount",
        "addedLines",
        "removedLines",
    ] {
        fields.insert(name.into(), FieldKind::Integer);
    }
    fields.insert("path".into(), FieldKind::ProjectPath);
    RecordSchema::new("aft/files/1", 1, fields)
}

pub(crate) fn object(value: Value) -> Map<String, Value> {
    value
        .as_object()
        .expect("display body is an object")
        .clone()
}

fn size(value: &Value) -> usize {
    canonical_body(value.as_object().expect("display object")).map_or(usize::MAX, |v| v.len())
}

/// Budget the escaped canonical document, including omission counters, not just
/// the unescaped text. Security omissions must be made before this size cut.
pub(crate) fn cut_detail(mut body: Value, limit: usize) -> Option<Value> {
    if size(&body) <= limit {
        return Some(body);
    }
    let mut omitted_bytes = 0usize;
    let mut omitted_items = 0usize;
    body["truncated"] = json!(true);
    body["omittedItems"] = json!(0);
    body["omittedBytes"] = json!(0);
    if body.get("files").is_some() {
        let files = body["files"].as_array_mut()?;
        // Stable path/operation order is retained. Drop diff content before an
        // entry, so a large diff cannot hide the other changed files.
        for file in files.iter_mut().rev() {
            if let Some(diff) = file.as_object_mut()?.remove("diff") {
                omitted_bytes += diff.as_str()?.len();
                file["diffOmitted"] = json!(true);
            }
        }
        body["omittedBytes"] = json!(omitted_bytes);
        while size(&body) > limit {
            let file = body["files"].as_array_mut()?.pop()?;
            if let Some(diff) = file.get("diff").and_then(Value::as_str) {
                omitted_bytes += diff.len();
            }
            omitted_items += 1;
            body["omittedItems"] = json!(omitted_items);
            body["omittedBytes"] = json!(omitted_bytes);
        }
    } else {
        let names = ["stderr", "stdout", "pty"];
        for name in names {
            while size(&body) > limit {
                let Some(text) = body.get(name).and_then(Value::as_str) else {
                    break;
                };
                if text.is_empty() {
                    break;
                }
                let mut keep = text.len().saturating_sub((size(&body) - limit).max(1));
                while !text.is_char_boundary(keep) {
                    keep -= 1;
                }
                omitted_bytes += text.len() - keep;
                let kept = text[..keep].to_owned();
                body[name] = json!(kept);
                body["omittedBytes"] = json!(omitted_bytes);
            }
        }
    }
    (size(&body) <= limit).then_some(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_role_tool_display::{
        schema::{validate_completion, validate_detail, validate_record},
        validate_write, DisplayKind, DisplayWrite,
    };

    #[test]
    fn every_design_example_has_exact_jcs_bytes_and_validates() {
        let examples: Vec<Value> = serde_json::from_str(include_str!(
            "../../../tests/fixtures/tool_views/display-examples.json"
        ))
        .unwrap();
        assert_eq!(examples.len(), 32);
        let pin = crate::subc::tool_views::SHIPPED.keys().next().unwrap();
        for example in examples {
            let schema = if example["schema"] == "bash" {
                bash_schema()
            } else {
                files_schema()
            };
            let body = example["body"].as_object().unwrap();
            let kind = match example["kind"].as_str().unwrap() {
                "record" => {
                    validate_record(body, &schema).unwrap();
                    DisplayKind::Record
                }
                "completion" => {
                    validate_completion(body, &schema).unwrap();
                    DisplayKind::Completion
                }
                "detail" => {
                    validate_detail(body).unwrap();
                    DisplayKind::Detail
                }
                _ => unreachable!(),
            };
            let expected = example["jcs"].as_str().unwrap().as_bytes();
            assert_eq!(canonical_body(body).unwrap(), expected);
            let write = DisplayWrite::new(
                subc_protocol::Principal::Direct,
                "example",
                kind,
                pin,
                body.clone(),
            );
            assert_eq!(
                validate_write(&write, &super::super::outbox::ceilings()).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn extension_fields_are_typed_and_standard_fields_are_never_redeclared() {
        for schema in [bash_schema(), files_schema()] {
            schema.validate().unwrap();
        }
        assert_eq!(bash_schema().fields["target"], FieldKind::Code);
        assert_eq!(bash_schema().fields["taskId"], FieldKind::Id);
        assert!(cortexkit_role_tool_display::schema::validate_record(
            &object(json!({"durationMs":0,"outcome":"success","target":"raw,8c"})),
            &bash_schema()
        )
        .is_err());
    }

    #[test]
    fn detail_byte_limit_is_canonical_and_marks_every_cut() {
        let body = json!({"schema":"aft/bash-detail/1","revision":1,"phase":"foreground_finished","stdout":"\"é\n".repeat(300),"stderr":""});
        let exact = size(&body);
        assert_eq!(cut_detail(body.clone(), exact), Some(body.clone()));
        let cut = cut_detail(body.clone(), exact - 1).unwrap();
        assert!(size(&cut) <= exact - 1);
        assert_eq!(cut["truncated"], true);
        assert_eq!(cut["omittedItems"], 0);
        let omitted =
            body["stdout"].as_str().unwrap().len() - cut["stdout"].as_str().unwrap().len();
        assert_eq!(cut["omittedBytes"], omitted);
        assert!(cut_detail(body, 2).is_none());
    }

    #[test]
    fn files_cut_drops_content_before_entries_and_budgets_cut_metadata() {
        let body = json!({"schema":"aft/files-detail/1","revision":1,"files":[{"change":"modified","path":"src/huge.rs","diff":"x".repeat(4_000_000)}]});
        let cut = cut_detail(body, 3_145_728).unwrap();
        assert_eq!(
            cut,
            json!({"schema":"aft/files-detail/1","revision":1,"files":[{"change":"modified","path":"src/huge.rs","diffOmitted":true}],"truncated":true,"omittedItems":0,"omittedBytes":4_000_000})
        );
        let smaller = cut_detail(json!({"schema":"aft/files-detail/1","revision":1,"files":[{"change":"modified","path":"long-name".repeat(200),"diff":"abcd"}]}), 150).unwrap();
        assert_eq!(smaller["omittedItems"], 1);
        assert_eq!(smaller["omittedBytes"], 4);
        assert!(size(&smaller) <= 150);
    }
}
