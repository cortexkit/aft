//! View templates answered through `tool.views`. Legacy pins keep their exact
//! views; versioned pins resolve to current or shipped tool definitions so
//! transcripts can still draw calls made against an older schema.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use cortexkit_role_tool_provider::{
    call::{SchemaPin, ToolMajorPin},
    catalog::{CatalogTool, ToolVersion},
    views::{ViewEntry, ViewManifest, ViewTool, ViewsReply, ViewsRequest},
};
use serde::Deserialize;
use serde_json::Value;
use subc_protocol::ErrorBody;

pub(super) mod current;
use super::rich_ui::notices;
use super::tool_provider::versions;

pub(crate) const BASH_SEMANTICS: u64 = 3;

#[derive(Clone, Deserialize, serde::Serialize)]
pub(crate) struct Definition {
    pub name: String,
    pub input_schema: Value,
    pub semantics: u64,
}
#[derive(Deserialize)]
pub(crate) struct Shipped {
    pub tool: Definition,
    pub history: Vec<ViewManifest>,
}

pub(crate) static SHIPPED: LazyLock<BTreeMap<String, Shipped>> = LazyLock::new(|| {
    let mut shipped: BTreeMap<String, Shipped> =
        serde_json::from_str(include_str!("tool_views/shipped.json"))
            .expect("committed view table");
    let mut bash_tail_views = Vec::new();
    for (pin, entry) in &mut shipped {
        let digest = cortexkit_role_tool_provider::catalog::schema_digest(&entry.tool.input_schema)
            .expect("shipped schema");
        assert_eq!(
            SchemaPin::new(&entry.tool.name, &digest, entry.tool.semantics)
                .encode()
                .expect("shipped pin"),
            *pin
        );
        current::append(entry);
        if entry.tool.name == "bash" {
            let tail_pin = SchemaPin::new("bash", &digest, BASH_SEMANTICS)
                .encode()
                .expect("bash tail pin");
            bash_tail_views.push((tail_pin, current::bash_tail_view(entry)));
        }
    }
    shipped.extend(bash_tail_views);
    shipped
});

pub(crate) fn has_bash_tail(pin: &str) -> bool {
    SHIPPED
        .get(pin)
        .or_else(|| versioned_source(pin).map(|(entry, ..)| entry))
        .is_some_and(|entry| entry.tool.name == "bash" && entry.tool.semantics == BASH_SEMANTICS)
}

pub(crate) fn view(pin: &str) -> Option<Cow<'static, ViewManifest>> {
    static VIEWS: LazyLock<BTreeMap<String, ViewManifest>> = LazyLock::new(|| {
        SHIPPED
            .keys()
            .chain(VERSIONED_VIEW_PINS.iter())
            .filter_map(|pin| build_view(pin).map(|view| (pin.clone(), view)))
            .collect()
    });
    VIEWS
        .get(pin)
        .map(Cow::Borrowed)
        .or_else(|| build_view(pin).map(Cow::Owned))
}

fn build_view(pin: &str) -> Option<ViewManifest> {
    if let Some(shipped) = SHIPPED.get(pin) {
        return Some(shipped.history.last()?.clone().with_tool(ViewTool::new(
            &shipped.tool.name,
            shipped.tool.input_schema.clone(),
            shipped.tool.semantics,
        )));
    }
    let (shipped, schema, major) = versioned_source(pin)?;
    // Minor and patch are catalog metadata, not part of a view's identity.
    let tool = ViewTool::new_versioned(
        &shipped.tool.name,
        schema.clone(),
        ToolVersion::new(major, 0, 0),
    );
    Some(shipped.history.last()?.clone().with_tool(tool))
}

fn versioned_source(encoded: &str) -> Option<(&'static Shipped, &'static Value, u64)> {
    let pin = ToolMajorPin::parse(encoded).ok()?;
    let (entry, schema) = versioned_source_for(&pin, versions::current_pin_tools())?;
    Some((entry, schema, pin.major))
}

fn versioned_source_for<'a>(
    pin: &ToolMajorPin,
    current: impl Iterator<Item = &'a CatalogTool>,
) -> Option<(&'static Shipped, &'a Value)> {
    let mut current = current
        .filter(|tool| {
            tool.name == pin.tool
                && tool
                    .version
                    .is_some_and(|version| version.major == pin.major)
        })
        .peekable();
    current.peek()?;

    if let Some(tool) = current.find(|tool| tool.schema_digest == pin.schema_digest) {
        let legacy = SchemaPin::new(&tool.name, &tool.schema_digest, tool.semantics)
            .encode()
            .ok()?;
        let entry = SHIPPED.get(&legacy).or_else(|| {
            // A current schema variant (such as hashline edit) can use the
            // same record template without having had its own legacy view.
            SHIPPED
                .values()
                .filter(|entry| entry.tool.name == tool.name)
                .max_by_key(|entry| entry.tool.semantics)
        })?;
        return Some((entry, &tool.input_schema));
    }

    let entry = SHIPPED
        .iter()
        .filter(|(encoded, entry)| {
            entry.tool.name == pin.tool
                && versions::shipped_major(encoded) == Some(pin.major)
                && SchemaPin::parse(encoded)
                    .is_ok_and(|legacy| legacy.schema_digest == pin.schema_digest)
        })
        .map(|(_, entry)| entry)
        .max_by_key(|entry| entry.tool.semantics)?;
    Some((entry, &entry.tool.input_schema))
}

static VERSIONED_VIEW_PINS: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    let current = versions::current_pin_tools().filter_map(|tool| {
        SHIPPED
            .values()
            .any(|entry| entry.tool.name == tool.name)
            .then(|| tool.schema_pin().encode().expect("current view pin"))
    });
    let historical = SHIPPED.keys().filter_map(|encoded| {
        let major = versions::shipped_major(encoded)?;
        let legacy = SchemaPin::parse(encoded).expect("shipped view pin");
        let pin = ToolMajorPin::new(legacy.tool, legacy.schema_digest, major)
            .encode()
            .expect("versioned view pin");
        versioned_source(&pin)?;
        Some(pin)
    });
    current.chain(historical).collect()
});

pub(crate) fn answer(body: Value) -> Result<Value, ErrorBody> {
    let request: ViewsRequest = serde_json::from_value(body).map_err(|_| {
        cortexkit_role_tool_provider::errors::invalid_request("pins", "expected pins array")
    })?;
    request.validate()?;
    let views = request
        .pins
        .iter()
        .map(|pin| {
            (
                pin.clone(),
                view(pin)
                    .map(|view| ViewEntry::Manifest(view.into_owned()))
                    .or_else(|| notices::view(pin).map(ViewEntry::Notice))
                    .unwrap_or(ViewEntry::Unknown),
            )
        })
        .collect();
    let reply = ViewsReply::new(views);
    reply.validate_for(&request)?;
    Ok(serde_json::to_value(reply).expect("view response"))
}

/// The management route serializes this same answer value without an envelope.
/// Definitions are immutable for a process; a catalog revision ships on restart.
pub(crate) fn bus_snapshot() -> &'static [(String, Vec<u8>)] {
    static SNAPSHOT: LazyLock<Vec<(String, Vec<u8>)>> = LazyLock::new(|| {
        SHIPPED
            .keys()
            .chain(notice_view_pins())
            .chain(VERSIONED_VIEW_PINS.iter())
            .map(|pin| {
                let reply = answer(serde_json::json!({"pins":[pin]})).expect("shipped pin request");
                (
                    pin.clone(),
                    serde_json::to_vec(&reply).expect("view response"),
                )
            })
            .collect()
    });
    &SNAPSHOT
}

pub(crate) fn notice_view_pins() -> impl Iterator<Item = &'static String> {
    notices::view_pins()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_role_tool_provider::{catalog::schema_digest, views::ViewChange};
    use serde_json::json;
    use sha2::{Digest, Sha256};

    #[test]
    fn pipeline_review_view_lookups_do_not_allocate_after_warmup() {
        let legacy = SHIPPED.keys().next().unwrap();
        let versioned = VERSIONED_VIEW_PINS.iter().next().unwrap();
        for pin in [legacy, versioned] {
            assert!(view(pin).is_some());
        }
        let (_, allocations) = crate::test_allocations::count(|| {
            for _ in 0..1000 {
                for pin in [legacy, versioned] {
                    assert!(std::hint::black_box(view(pin)).is_some());
                }
            }
        });
        assert_eq!(allocations, 0, "warm display lookups rebuilt views");
    }

    #[test]
    fn pipeline_review_bus_snapshots_do_not_allocate_after_warmup() {
        assert!(!bus_snapshot().is_empty());
        let (_, allocations) = crate::test_allocations::count(|| {
            for _ in 0..3 {
                assert!(!std::hint::black_box(bus_snapshot()).is_empty());
            }
        });
        assert_eq!(allocations, 0, "warm bus ticks rebuilt view snapshots");
    }

    fn assert_template_digests(template: &Value, expected: [&str; 2]) {
        for (slot, digest) in ["row", "page"].into_iter().zip(expected) {
            let canonical =
                cortexkit_role_tool_display::canonical_body(template[slot].as_object().unwrap())
                    .unwrap();
            assert_eq!(format!("{:x}", Sha256::digest(canonical)), digest, "{slot}");
        }
    }

    #[test]
    fn versioned_views_select_shipped_history_but_refuse_unknown_digests_and_majors() {
        let legacy =
            "tp1:apply_patch:fa62c9797e5d14436714f5630d15815d2adc1185023a3dcba3f3ed20fc9f0548:1";
        let old = SchemaPin::parse(legacy).unwrap();
        let pin = ToolMajorPin::new(&old.tool, &old.schema_digest, 1);
        let schema = json!({"type":"object", "properties":{"different": {"type":"string"}}});
        let newer = CatalogTool::new(&old.tool, schema_digest(&schema).unwrap(), 1, schema)
            .with_version(ToolVersion::new(1, 1, 0));
        assert_ne!(newer.schema_digest, pin.schema_digest);
        let (entry, schema) = versioned_source_for(&pin, std::iter::once(&newer)).unwrap();
        assert!(std::ptr::eq(entry, &SHIPPED[legacy]));
        assert_eq!(*schema, SHIPPED[legacy].tool.input_schema);

        let unknown_digest = ToolMajorPin::new(&old.tool, "0".repeat(64), 1)
            .encode()
            .unwrap();
        let unknown_major = ToolMajorPin::new(&old.tool, &old.schema_digest, 99)
            .encode()
            .unwrap();
        let response = answer(json!({"pins":[unknown_digest,unknown_major]})).unwrap();
        assert_eq!(response["views"][&unknown_digest], "unknown");
        assert_eq!(response["views"][&unknown_major], "unknown");
    }

    #[test]
    fn versioned_views_include_the_current_hashline_edit_schema() {
        let hashline = versions::current_pin_tools()
            .find(|tool| {
                tool.name == "edit" && tool.input_schema["properties"].get("patch").is_some()
            })
            .unwrap();
        let pin = hashline.schema_pin().encode().unwrap();
        assert!(VERSIONED_VIEW_PINS.contains(&pin));
        let view = view(&pin).unwrap();
        assert_eq!(view.template_revision, 5);
        assert!(view.check_pin(&pin).unwrap());
        assert_eq!(
            view.tool.as_ref().unwrap().input_schema,
            hashline.input_schema
        );
    }

    #[test]
    fn every_shipped_pin_remains_servable() {
        let baseline: BTreeMap<String, Shipped> = serde_json::from_str(include_str!(
            "../../tests/fixtures/tool_views/shipped-v1.json"
        ))
        .unwrap();
        assert!(!baseline.is_empty());
        let pins: Vec<_> = baseline.keys().collect();
        let answer = answer(json!({"pins":pins})).unwrap();
        for (pin, kept) in baseline {
            assert_ne!(
                answer["views"][&pin], "unknown",
                "served pin disappeared: {pin}"
            );
            let fetched: ViewManifest =
                serde_json::from_value(answer["views"][&pin].clone()).unwrap();
            assert_eq!(
                answer["views"][&pin]["tool"],
                serde_json::to_value(&kept.tool).unwrap(),
                "first served definition changed for {pin}"
            );
            for old in kept.history {
                let catalog_refresh = &SHIPPED[&pin].history[0];
                assert_ne!(
                    catalog_refresh.template, old.template,
                    "catalog refresh must change the published template bytes for {pin}"
                );
                assert_eq!(catalog_refresh.template_revision, old.template_revision + 1);
                assert_eq!(
                    fetched.compare_with_kept(&old),
                    ViewChange::NewerTemplate,
                    "changed template must be accepted as a newer revision for {pin}"
                );
                let mut expected_template = old.template;
                for slot in ["row", "page"] {
                    expected_template[slot]["catalog"]["digest"] =
                        catalog_refresh.template[slot]["catalog"]["digest"].clone();
                }
                assert_eq!(
                    catalog_refresh.template, expected_template,
                    "template changed: {pin}"
                );
            }
        }
    }

    #[test]
    fn each_stored_definition_recomputes_its_literal_pin() {
        for (pin, entry) in SHIPPED.iter() {
            let digest = schema_digest(&entry.tool.input_schema).unwrap();
            assert_eq!(
                SchemaPin::new(&entry.tool.name, digest, entry.tool.semantics)
                    .encode()
                    .unwrap(),
                *pin
            );
            for pair in entry.history.windows(2) {
                assert_eq!(
                    pair[1].compare_with_kept(&pair[0]),
                    ViewChange::NewerTemplate
                );
            }
        }
    }

    #[test]
    fn latest_views_use_the_current_catalog_and_preserve_the_record_schema() {
        for (pin, entry) in SHIPPED.iter() {
            let view = view(pin).unwrap();
            let mut schema = if entry.tool.name == "bash" {
                crate::subc::rich_ui::records::bash_schema()
            } else {
                crate::subc::rich_ui::records::files_schema()
            };
            if has_bash_tail(pin) {
                assert_eq!(view.template_revision, 2);
                schema.revision = 2;
                schema.completion_fields.insert(
                    "tail".into(),
                    cortexkit_role_tool_display::FieldKind::TextLines,
                );
            } else if matches!(entry.tool.name.as_str(), "edit" | "write" | "apply_patch") {
                assert_eq!(view.template_revision, 5);
            } else {
                assert_eq!(view.template_revision, 3);
            }
            assert_eq!(view.record_schema, schema);
            assert!(view.check_pin(pin).unwrap());
            assert_eq!(
                serde_json::to_value(view.tool.as_ref().unwrap()).unwrap(),
                json!({"name":entry.tool.name,"input_schema":entry.tool.input_schema,"semantics":entry.tool.semantics})
            );
            for (slot, profile) in [("row", "tool_row"), ("page", "tool_page")] {
                let template = &view.template[slot];
                assert_eq!(template["schema"], "occipital.template/1");
                assert_eq!(template["profile"], profile);
                assert_eq!(
                    template["catalog"],
                    json!({
                        "version": "1.1",
                        "digest": current::CATALOG_DIGEST
                    })
                );
                assert_eq!(template["reads"], json!([]));
                assert!(
                    cortexkit_role_tool_display::canonical_body(template.as_object().unwrap())
                        .unwrap()
                        .len()
                        <= 131_072
                );
            }
            let page = view.template["page"]["source"].as_str().unwrap();
            assert!(page.contains(match entry.tool.name.as_str() {
                "edit" => "view.detail.hunks",
                "write" => "codeString={input.content}",
                "apply_patch" => "view.detail.files",
                _ => "view.output ??",
            }));
            // Revision 5 changes the row only; revision 4 keeps its published bytes.
            let (published_row, current_row, page) = match entry.tool.name.as_str() {
                "apply_patch" => (
                    "3ec957da89287b0d7abaef752ddd79a862bb83c74f0fc0b7eb11af193e73193d",
                    "5cdbbdd92a8368d6bc9780a9b7095bb92e67c621c005f20db689032f8b8748de",
                    "58f52d34e95b95f474281175d3790627a4f9263e87a078b8b1fefef80116688b",
                ),
                "edit" => (
                    "0fea838e52c7f79185fa7892f1605fd6be7b2c4e97ec93e9c0543ef95807f531",
                    "7c5c2d1cbaacfafc3f7c9730fd25e3ef72a733780a0be53c78bec817c3faa72f",
                    "3a457fd30d60ca7acc5bab258da9786bc309a44190d5454795f2b80a870332d7",
                ),
                "write" => (
                    "0fea838e52c7f79185fa7892f1605fd6be7b2c4e97ec93e9c0543ef95807f531",
                    "7c5c2d1cbaacfafc3f7c9730fd25e3ef72a733780a0be53c78bec817c3faa72f",
                    "3422ad764d8aea87c95e6f6b38e21bf10096d27ef72bedb07a9f4276b8fa23fd",
                ),
                "bash" => continue, // Covered by the tail-pin compatibility test.
                name => panic!("missing template digests for {name}"),
            };
            let published = &entry.history[entry.history.len() - 2];
            assert_eq!(published.template_revision, 4);
            assert_eq!(published.record_schema, view.record_schema);
            assert_template_digests(&published.template, [published_row, page]);
            assert_template_digests(&view.template, [current_row, page]);
        }
    }

    #[test]
    fn bash_catalog_serves_a_new_tail_pin_without_changing_the_old_record_schema() {
        use crate::subc::tool_provider::{tools_for_session, CatalogPreset};

        // Occipital's template checker (occipital-check 0.1.7) reported these
        // canonical hashes and verified the row height for each Bash outcome.
        let legacy_template_digests = [
            "291990ef28f5790ab9d2d21fea9a0d5c620f6bebb9c66a3cda9a92d11cf3b924",
            "e8d1495e42c90404af114aaa5e369d1e57e24571174798dd974589c3a07e173a",
        ];
        // The Occipital checker hashes the entire canonical template document,
        // which names the tool catalog. These are the one-line row and the
        // unchanged page re-declared on catalog 6e323928…, as passed by
        // occipital-check from occipital 6771d71.
        let tail_template_digests = [
            "291990ef28f5790ab9d2d21fea9a0d5c620f6bebb9c66a3cda9a92d11cf3b924",
            "448495cc4f693bb622f407532eabddae4fdfc55a249de00eec08ab13eef2a658",
        ];

        let base: BTreeMap<String, Shipped> = serde_json::from_str(include_str!(
            "../../tests/fixtures/tool_views/shipped-v1.json"
        ))
        .unwrap();
        let sample: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/tool_views/phone-reader-tool-tail-v1/02-views.json"
        ))
        .unwrap();
        let sample_schema = &sample["result"]["views"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()["recordSchema"];
        for remote in [false, true] {
            let bash = tools_for_session(CatalogPreset::Head, &[], true, remote, false)
                .into_iter()
                .find(|tool| tool.name == "bash")
                .unwrap();
            assert_eq!(bash.semantics, 3);
            let pin = SchemaPin::new("bash", &bash.schema_digest, bash.semantics)
                .encode()
                .unwrap();
            let old = SchemaPin::new("bash", &bash.schema_digest, 2)
                .encode()
                .unwrap();
            let reply = answer(json!({"pins":[pin,old]})).unwrap();
            assert_eq!(
                reply["views"][&old]["record_schema"],
                serde_json::to_value(&base[&old].history[0].record_schema).unwrap(),
            );
            let schema = &reply["views"][&pin]["record_schema"];
            assert_eq!(
                schema["completion_fields"],
                sample_schema["completion_fields"]
            );
            assert_eq!(
                schema["fields"],
                reply["views"][&old]["record_schema"]["fields"]
            );
            assert_template_digests(&reply["views"][&old]["template"], legacy_template_digests);
            assert_template_digests(&reply["views"][&pin]["template"], tail_template_digests);
        }
    }

    #[test]
    fn published_view_tool_vectors_match_mismatch_and_legacy() {
        let vectors: Value =
            serde_json::from_str(cortexkit_role_tool_provider::vectors::VIEW_TOOLS_JSON).unwrap();
        let cases = vectors["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 3);
        for case in cases {
            let manifest: ViewManifest = serde_json::from_value(case["view"].clone()).unwrap();
            let pin = case["pin"].as_str().unwrap();
            match case["check"].as_str().unwrap() {
                "present" => assert!(manifest.check_pin(pin).unwrap()),
                "mismatch" => assert!(manifest.check_pin(pin).is_err()),
                "absent" => {
                    assert!(!manifest.check_pin(pin).unwrap());
                    assert_eq!(
                        serde_json::to_string(&manifest).unwrap(),
                        case["encoded"].as_str().unwrap()
                    );
                }
                _ => panic!("unexpected shared vector"),
            }
            if let Some(tool) = manifest.tool {
                assert_eq!(
                    tool.schema_pin().unwrap(),
                    case["computed_pin"].as_str().unwrap()
                );
            }
        }
    }

    #[test]
    fn views_requests_validate_and_return_exactly_distinct_keys() {
        for body in [json!({}), json!({"pins":null}), json!({"pins":"x"})] {
            assert!(answer(body).is_err());
        }
        assert_eq!(
            answer(json!({"pins":[""]})).unwrap_err().detail.unwrap()["field"],
            "pins[0]"
        );
        assert!(answer(json!({"pins":vec!["x";65]})).is_err());
        assert_eq!(answer(json!({"pins":[]})).unwrap(), json!({"views":{}}));
        assert_eq!(
            answer(json!({"pins":["future", "future"]})).unwrap(),
            json!({"views":{"future":"unknown"}})
        );
    }

    #[test]
    #[ignore = "exports checker inputs without rewriting the permanent table"]
    fn export_occipital_check_inputs() {
        let dir = std::path::Path::new(crate::test_support::manifest_dir())
            .join("../../target/tool-views-check");
        std::fs::create_dir_all(&dir).unwrap();
        let file_samples: BTreeMap<String, Value> = serde_json::from_str(include_str!(
            "../../tests/fixtures/tool_views/write-and-apply-patch-samples.json"
        ))
        .unwrap();
        for (index, (pin, entry)) in SHIPPED.iter().enumerate() {
            let name = &entry.tool.name;
            let (filename, samples) = match name.as_str() {
                "bash" => (
                    format!("{index}.json"),
                    json!(bash_checker_samples(
                        &json!({"command":"printf ok"}),
                        has_bash_tail(pin)
                    )),
                ),
                "edit" => (
                    "edit.json".into(),
                    json!([{
                        "name":"edit-one-file",
                        "input":{"path":"src/a.rs","edits":[{"oldString":"old","newString":"new"}]},
                        "record":{"durationMs":1,"outcome":"held","reason":"consent"},
                        "completion":{"durationMs":12,"heldMs":2000,"outcome":"success","fileCount":1,"modifiedCount":1,"createdCount":0,"deletedCount":0,"movedCount":0,"addedLines":1,"removedLines":1,"path":"src/a.rs"},
                        "detail":{"schema":"aft/files-detail/1","revision":1,"files":[{"change":"modified","path":"src/a.rs"}],"hunks":[{"title":"Change 1 · line 1","oldStart":1,"newStart":1,"lines":[{"kind":"delete","text":"old"},{"kind":"add","text":"new"}]}]},
                    }]),
                ),
                "write" | "apply_patch" => (format!("{name}.json"), file_samples[name].clone()),
                _ => unreachable!(),
            };
            let document = json!({"input_schema":entry.tool.input_schema,"view":view(pin).unwrap(),"samples":samples});
            std::fs::write(dir.join(filename), serde_json::to_vec(&document).unwrap()).unwrap();
        }
    }

    fn bash_checker_samples(input: &Value, has_tail: bool) -> Vec<Value> {
        let mut samples = Vec::new();
        for (end_reason, exit_code, outcome) in [
            ("exited", Some(0), "success"),
            ("exited", Some(1), "error"),
            ("timed_out", Some(124), "error"),
            ("killed", None, "error"),
        ] {
            let mut record = json!({"durationMs":8,"outcome":outcome,"phase":"foreground_finished","outputBytes":3,"endReason":end_reason,"execution":"local"});
            if let Some(code) = exit_code {
                record["exitCode"] = json!(code);
            }
            samples.push(json!({"input":input,"record":record}));
        }
        let record = json!({"durationMs":8,"outcome":"success","phase":"background_started","taskId":"bash-demo1","outputBytes":0,"execution":"local"});
        let mut completion = json!({"outcome":"error","phase":"background_finished","taskId":"bash-demo1","exitCode":1,"runDurationMs":2400,"outputBytes":3,"endReason":"exited","execution":"local"});
        if has_tail {
            completion["tail"] = json!([
                {"text":"running 12 tests"},
                {"text":"test result: FAILED. 11 passed; 1 failed","tone":"danger"},
            ]);
        }
        samples.push(json!({"input":input,"record":record,"completion":completion}));
        samples
    }
}
