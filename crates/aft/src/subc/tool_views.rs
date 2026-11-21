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

pub(super) mod current;
use super::rich_ui::notices;

pub(crate) const BASH_SEMANTICS: u64 = 3;

/// Earlier `bash` behavior versions whose calls still run. Version 3 only added
/// the `tail` completion field to the app record; the command's input and
/// behavior are the same as version 2. A session keeps the version its plan
/// named, and a worker keeps its tool list for its whole run, so refusing
/// version 2 would take `bash` away from every running session until its
/// tool list refreshed. A call pinned to version 2 runs as before, and its
/// record is drawn with the version-2 view, which has no `tail`.
const BASH_RECORD_ONLY_PREDECESSORS: &[u64] = &[2];

/// Whether a call pinned to `pinned` may run against the current `semantics`
/// of `tool` because the versions differ only in what the app record carries.
pub(crate) fn runs_under_newer_semantics(tool: &str, pinned: u64, semantics: u64) -> bool {
    tool == "bash" && semantics == BASH_SEMANTICS && BASH_RECORD_ONLY_PREDECESSORS.contains(&pinned)
}

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
                    .map(ViewEntry::Manifest)
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
pub(crate) fn bus_snapshot() -> Vec<(String, Vec<u8>)> {
    SHIPPED
        .keys()
        .chain(notice_view_pins())
        .map(|pin| {
            let reply = answer(serde_json::json!({"pins":[pin]})).expect("shipped pin request");
            (
                pin.clone(),
                serde_json::to_vec(&reply).expect("view response"),
            )
        })
        .collect()
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

    fn assert_template_digests(template: &Value, expected: [&str; 2]) {
        for (slot, digest) in ["row", "page"].into_iter().zip(expected) {
            let canonical =
                cortexkit_role_tool_display::canonical_body(template[slot].as_object().unwrap())
                    .unwrap();
            assert_eq!(format!("{:x}", Sha256::digest(canonical)), digest, "{slot}");
        }
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
            } else if matches!(entry.tool.name.as_str(), "write" | "apply_patch") {
                assert_eq!(view.template_revision, 4);
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
                "edit" | "write" => "view.detail.hunks",
                "apply_patch" => "view.detail.files",
                _ => "view.output ??",
            }));
            // Canonical row and page SHA-256s reported by occipital-check
            // 0.1.7. The write and apply_patch samples were checked at phone
            // and desktop widths.
            let digests = match entry.tool.name.as_str() {
                "apply_patch" => [
                    "e13744cb5e3fe03a188391d6245127f58d0b1b69884e5793656ac2186f64f0c6",
                    "2acf86a6d82f3e8992005d8ab24840ea0fbdd41260a4bb643177640210854e9d",
                ],
                "edit" | "write" => [
                    "f245c72b27b00e954216045081ed1b2721ac52fe2055ce8f927c794e253a6a6e",
                    "3a457fd30d60ca7acc5bab258da9786bc309a44190d5454795f2b80a870332d7",
                ],
                "bash" => continue, // Covered by the tail-pin compatibility test.
                name => panic!("missing template digests for {name}"),
            };
            assert_template_digests(&view.template, digests);
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
                    format!("{index}.json"),
                    json!([{
                        "input":{"path":"src/a.rs","edits":[{"oldString":"old","newString":"new"}]},
                        "record":{"durationMs":1,"outcome":"held","reason":"consent"},
                        "completion":{"durationMs":12,"heldMs":2000,"outcome":"success","fileCount":1,"modifiedCount":1,"createdCount":0,"deletedCount":0,"movedCount":0,"addedLines":1,"removedLines":1,"path":"src/a.rs"},
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
