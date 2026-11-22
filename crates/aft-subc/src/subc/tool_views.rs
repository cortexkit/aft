//! View templates answered through `tool.views`, keyed by the exact schema pin
//! (the hash of one tool definition) each was served for. Once served, a pin
//! stays here even after the catalog moves to a newer definition, so a
//! transcript's older calls can still be drawn.
use std::collections::BTreeMap;
use std::sync::LazyLock;

use cortexkit_role_tool_provider::{
    call::SchemaPin,
    views::{ViewEntry, ViewManifest, ViewsReply, ViewsRequest},
};
use serde::Deserialize;
use serde_json::Value;
use subc_protocol::ErrorBody;

mod current;

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
        .is_some_and(|entry| entry.tool.name == "bash" && entry.tool.semantics == BASH_SEMANTICS)
}

pub(crate) fn view(pin: &str) -> Option<ViewManifest> {
    let shipped = SHIPPED.get(pin)?;
    let digest =
        cortexkit_role_tool_provider::catalog::schema_digest(&shipped.tool.input_schema).ok()?;
    let catalog = cortexkit_role_tool_provider::catalog::CatalogTool::new(
        shipped.tool.name.clone(),
        digest,
        shipped.tool.semantics,
        shipped.tool.input_schema.clone(),
    );
    Some(shipped.history.last()?.clone().with_tool(
        cortexkit_role_tool_provider::views::ViewTool::from(&catalog),
    ))
}

pub(super) fn answer(body: Value) -> Result<Value, ErrorBody> {
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
                view(pin).map_or(ViewEntry::Unknown, ViewEntry::Manifest),
            )
        })
        .collect();
    let reply = ViewsReply::new(views);
    reply.validate_for(&request)?;
    Ok(serde_json::to_value(reply).expect("view response"))
}

/// The management route serializes this same answer value without an envelope.
/// Definitions are immutable for a process; a catalog revision ships on restart.
pub(crate) fn bus_snapshot() -> Vec<(String, Vec<u8>)> {
    SHIPPED
        .keys()
        .map(|pin| {
            let reply = answer(serde_json::json!({"pins":[pin]})).expect("shipped pin request");
            (
                pin.clone(),
                serde_json::to_vec(&reply).expect("view response"),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_role_tool_provider::{catalog::schema_digest, views::ViewChange};
    use serde_json::json;

    #[test]
    fn every_shipped_pin_remains_servable() {
        let baseline: BTreeMap<String, Shipped> = serde_json::from_str(include_str!(
            "../../../aft/tests/fixtures/tool_views/shipped-v1.json"
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
                assert_eq!(view.template_revision, 1);
                schema.revision = 2;
                schema.completion_fields.insert(
                    "tail".into(),
                    cortexkit_role_tool_display::FieldKind::TextLines,
                );
            } else {
                assert_eq!(view.template_revision, 2);
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
            assert!(page.contains(if entry.tool.name == "edit" {
                "view.detail.hunks"
            } else {
                "view.output ??"
            }));
        }
    }

    #[test]
    fn bash_catalog_serves_a_new_tail_pin_without_changing_the_old_record_schema() {
        use crate::subc::tool_provider::{tools_for_session, CatalogPreset};
        use sha2::{Digest, Sha256};

        // These source digests were served before the completion schema had a tail.
        let legacy_template_digests = [
            (
                "row",
                "f735ae10c6e774181736e0f126c6250a37ac37c7a80ef77d7cef0ed89f6b553a",
            ),
            (
                "page",
                "ec82b0c56449e389316211a1a5da560e2fb9c680ad36bd901762e8d22b80623b",
            ),
        ];
        // The Occipital checker hashes the entire canonical template document.
        // The row is the one-line row (icon, title, narrow Failed mark) that
        // OCCI re-ran through occipital-check on 20 Nov; the page is unchanged
        // from the phone-reader-tool-tail-v1 sample.
        let tail_template_digests = [
            "8444993fa225504435700391cbcc97b7fd1f641622724106388bd89bd72199f2",
            "d0f88d63925c8d289d6f273a7a9f5743e67f49f24e5d7020536cf254f40c9cb4",
        ];

        let base: BTreeMap<String, Shipped> = serde_json::from_str(include_str!(
            "../../../aft/tests/fixtures/tool_views/shipped-v1.json"
        ))
        .unwrap();
        let sample: Value = serde_json::from_str(include_str!(
            "../../../aft/tests/fixtures/tool_views/phone-reader-tool-tail-v1/02-views.json"
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
            for ((slot, legacy_digest), tail_digest) in legacy_template_digests
                .into_iter()
                .zip(tail_template_digests)
            {
                let legacy_source = reply["views"][&old]["template"][slot]["source"]
                    .as_str()
                    .unwrap();
                assert_eq!(
                    format!("{:x}", Sha256::digest(legacy_source.as_bytes())),
                    legacy_digest
                );
                let template = reply["views"][&pin]["template"][slot].as_object().unwrap();
                let canonical = cortexkit_role_tool_display::canonical_body(template).unwrap();
                assert_eq!(
                    format!("{:x}", Sha256::digest(canonical)),
                    tail_digest,
                    "{slot}"
                );
            }
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
        for (index, (pin, entry)) in SHIPPED.iter().enumerate() {
            let input = match entry.tool.name.as_str() {
                "bash" => json!({"command":"printf ok"}),
                "edit" => {
                    json!({"path":"src/a.rs","edits":[{"oldString":"old","newString":"new"}]})
                }
                "write" => json!({"path":"src/a.rs","content":"new\n"}),
                "apply_patch" => {
                    json!({"patchText":"*** Begin Patch\n*** Add File: src/a.rs\n+new\n*** End Patch"})
                }
                _ => unreachable!(),
            };
            let (record, mut completion) = if entry.tool.name == "bash" {
                (
                    json!({"durationMs":8,"outcome":"success","phase":"background_started","taskId":"bash-demo1","outputBytes":0,"execution":"local"}),
                    json!({"outcome":"success","phase":"background_finished","taskId":"bash-demo1","exitCode":0,"runDurationMs":2400,"outputBytes":3,"endReason":"exited","execution":"local"}),
                )
            } else {
                (
                    json!({"durationMs":1,"outcome":"held","reason":"consent"}),
                    json!({"durationMs":12,"heldMs":2000,"outcome":"success","fileCount":1,"modifiedCount":1,"createdCount":0,"deletedCount":0,"movedCount":0,"addedLines":1,"removedLines":1,"path":"src/a.rs"}),
                )
            };
            if has_bash_tail(pin) {
                completion["tail"] = json!([
                    {"text":"running 12 tests"},
                    {"text":"test result: ok. 12 passed; 0 failed","tone":"muted"},
                    {"text":"warning: 1 test took over 60 s","tone":"danger"},
                ]);
            }
            let document = json!({"input_schema":entry.tool.input_schema,"view":view(pin).unwrap(),"samples":[{"input":input,"record":record,"completion":completion}]});
            std::fs::write(
                dir.join(format!("{index}.json")),
                serde_json::to_vec(&document).unwrap(),
            )
            .unwrap();
        }
    }
}

#[cfg(test)]
#[path = "tool_views_bus_tests.rs"]
mod bus_tests;
