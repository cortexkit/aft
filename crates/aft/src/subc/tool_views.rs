//! View templates answered through `tool.views`, keyed by the exact schema pin
//! (the hash of one tool definition) each was served for. Once served, a pin
//! stays here even after the catalog moves to a newer definition, so a
//! transcript's older calls can still be drawn.
use std::collections::BTreeMap;
use std::sync::LazyLock;

use cortexkit_role_tool_provider::views::{ViewEntry, ViewManifest, ViewsReply, ViewsRequest};
use serde::Deserialize;
use serde_json::Value;
use subc_protocol::ErrorBody;

#[derive(Deserialize, serde::Serialize)]
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
    let shipped: BTreeMap<String, Shipped> =
        serde_json::from_str(include_str!("tool_views/shipped.json"))
            .expect("committed view table");
    for (pin, entry) in &shipped {
        let digest = cortexkit_role_tool_provider::catalog::schema_digest(&entry.tool.input_schema)
            .expect("shipped schema");
        assert_eq!(
            cortexkit_role_tool_provider::call::SchemaPin::new(
                &entry.tool.name,
                digest,
                entry.tool.semantics
            )
            .encode()
            .expect("shipped pin"),
            *pin
        );
    }
    shipped
});

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

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_role_tool_provider::{
        call::SchemaPin, catalog::schema_digest, views::ViewChange,
    };
    use serde_json::json;

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
                assert_eq!(fetched.template_revision, old.template_revision + 1);
                assert!(
                    matches!(
                        fetched.compare_with_kept(&old),
                        ViewChange::Same | ViewChange::NewerTemplate
                    ),
                    "changed frozen view: {pin}"
                );
                let mut expected_template = old.template;
                for slot in ["row", "page"] {
                    expected_template[slot]["catalog"]["digest"] =
                        fetched.template[slot]["catalog"]["digest"].clone();
                }
                assert_eq!(fetched.template, expected_template, "template changed: {pin}");
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
    fn placeholder_views_have_current_revision_and_null_safe_pages() {
        for (pin, entry) in SHIPPED.iter() {
            let view = view(pin).unwrap();
            assert_eq!(view.template_revision, 1);
            assert_eq!(
                view.record_schema,
                if entry.tool.name == "bash" {
                    crate::subc::rich_ui::records::bash_schema()
                } else {
                    crate::subc::rich_ui::records::files_schema()
                }
            );
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
                        "digest": "sha256:a95693cfabc0bf2d7e7276fd1c634128d96c11605c1cdfe480da9ac2418683a4"
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
            assert!(view.template["page"]["source"]
                .as_str()
                .unwrap()
                .contains("view.output ??"));
            assert!(!view.template["page"]["source"]
                .as_str()
                .unwrap()
                .contains("view.detail"));
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
            let (record, completion) = if entry.tool.name == "bash" {
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
            let document = json!({"input_schema":entry.tool.input_schema,"view":view(pin).unwrap(),"samples":[{"input":input,"record":record,"completion":completion}]});
            std::fs::write(
                dir.join(format!("{index}.json")),
                serde_json::to_vec(&document).unwrap(),
            )
            .unwrap();
        }
    }
}
