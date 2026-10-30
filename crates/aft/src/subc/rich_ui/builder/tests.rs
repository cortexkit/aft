use super::*;
use crate::subc::tool_provider::{tools_for_session, CatalogPreset};
use cortexkit_role_tool_provider::call::SchemaPin;

fn fixture(name: &str) -> Value {
    macro_rules! fixtures {
        ($($file:literal),+ $(,)?) => {
            match name {
                $($file => &include_bytes!(concat!("../../../../tests/fixtures/rich_ui/", $file))[..],)+
                _ => panic!("unknown freeze fixture {name}"),
            }
        };
    }
    let bytes = fixtures!(
        "placeholder-row.json",
        "placeholder-page.json",
        "bash-pins.json",
        "field-table.json",
        "result-vectors.json",
        "handler-replies.json",
        "media-outcomes.json",
        "gate-contract.json",
        "target-arguments.json",
        "bound-task.json",
        "transport-contract.json",
        "view-record.json",
        "bash-inputs.json",
        "bash-output.json"
    );
    let mut value: Value = serde_json::from_slice(bytes).unwrap();
    if name == "bash-output.json" {
        for vector in value["vectors"].as_array_mut().unwrap() {
            for raw in vector["raw"]
                .as_object_mut()
                .into_iter()
                .flat_map(|raw| raw.values_mut())
            {
                if raw.is_object() {
                    // Only input bytes are encoded. Expected page text, splits
                    // and cursor fields remain independently frozen in JSON.
                    let mut expanded = vec![
                        raw["repeatByte"].as_u64().unwrap() as u8;
                        raw["count"].as_u64().unwrap() as usize
                    ];
                    expanded.extend(
                        raw["suffix"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|byte| byte.as_u64().unwrap() as u8),
                    );
                    use sha2::{Digest, Sha256};
                    assert_eq!(
                        format!("{:x}", Sha256::digest(&expanded)),
                        raw["sha256"].as_str().unwrap()
                    );
                    *raw = json!(expanded);
                }
            }
        }
    }
    value
}

fn bash_pins() -> Vec<String> {
    let mut pins = Vec::new();
    for preset in [CatalogPreset::Head, CatalogPreset::Worker] {
        for runon in [false, true] {
            for tool in tools_for_session(preset, &[], true, runon, false) {
                if tool.name == "bash" {
                    pins.push(
                        SchemaPin::new("bash", &tool.schema_digest, 2)
                            .encode()
                            .unwrap(),
                    );
                }
            }
        }
    }
    pins.sort();
    pins.dedup();
    pins
}

fn inputs() -> (Value, Vec<Value>, Vec<Value>, Vec<Template>) {
    let templates = ["placeholder-row.json", "placeholder-page.json"]
        .into_iter()
        .map(|name| template(fixture(name)).unwrap())
        .collect::<Vec<_>>();
    let views = vec![json!({"tool": "bash", "definitionDigests": bash_pins(),
        "viewRecord": {"schema": "aft.bash.view", "version": 1},
        "row": {"template": templates[0].digest},
        "page": {"template": templates[1].digest, "reads": templates[1].value["reads"]},
    })];
    (
        templates[0].value["catalog"].clone(),
        operations::operations(),
        views,
        templates,
    )
}

fn manifest() -> Manifest {
    let (catalog, operations, views, templates) = inputs();
    build(catalog, operations, views, templates).unwrap()
}

#[test]
fn golden_manifest_and_digest_are_platform_independent() {
    let built = manifest();
    for (profile, bytes) in [
        (
            "tool_row",
            &include_bytes!("../../../../tests/fixtures/rich_ui/placeholder-row.json")[..],
        ),
        (
            "tool_page",
            &include_bytes!("../../../../tests/fixtures/rich_ui/placeholder-page.json")[..],
        ),
    ] {
        assert_eq!(
            built
                .templates
                .iter()
                .find(|t| t.value["profile"] == profile)
                .unwrap()
                .bytes,
            bytes
        );
    }
    if std::env::var_os("AFT_RICH_UI_PRINT_FREEZE").is_some() {
        println!(
            "FREEZE_MANIFEST={}",
            String::from_utf8(built.bytes.clone()).unwrap()
        );
        println!("FREEZE_DIGEST={}", built.digest);
        println!(
            "FREEZE_PINS={}",
            built.value["views"][0]["definitionDigests"]
        );
    }
    assert_eq!(
        built.bytes,
        include_bytes!("../../../../tests/fixtures/rich_ui/manifest.jcs.json")
    );
    assert_eq!(
        built.digest,
        include_str!("../../../../tests/fixtures/rich_ui/manifest.sha256").trim()
    );
    assert_eq!(
        built.value["views"][0]["definitionDigests"],
        fixture("bash-pins.json")
    );
    assert_eq!(
        built.value["views"][0]["page"]["reads"],
        built
            .templates
            .iter()
            .find(|t| t.value["profile"] == "tool_page")
            .unwrap()
            .value["reads"]
    );
}

#[test]
fn catalog_and_template_order_do_not_change_bytes() {
    let expected = manifest().bytes;
    let (catalog, mut operations, mut views, mut templates) = inputs();
    operations.reverse();
    templates.reverse();
    views[0]["definitionDigests"]
        .as_array_mut()
        .unwrap()
        .reverse();
    views[0]["page"]["reads"].as_array_mut().unwrap().reverse();
    assert_eq!(
        expected,
        build(catalog, operations, views, templates).unwrap().bytes
    );
}

#[test]
fn runtime_catalog_filters_are_not_builder_inputs() {
    let expected = manifest().bytes;
    for revision_reads in [false, true] {
        let filtered = tools_for_session(
            CatalogPreset::Head,
            &["bash".into(), "read".into()],
            false,
            false,
            revision_reads,
        );
        assert!(!filtered.iter().any(|tool| tool.name == "bash"));
        assert_eq!(expected, manifest().bytes);
    }
    // The public provider version is a reviewed literal, not CARGO_PKG_VERSION.
    assert_eq!(manifest().value["providerVersion"], "1.0");
}

#[test]
fn frozen_fields_and_result_vectors_match_each_operation() {
    let table = fixture("field-table.json");
    let vectors = fixture("result-vectors.json");
    let ops = operations::operations();
    assert_eq!(ops.len(), 10);
    assert_eq!(table.as_object().unwrap().len(), 10);
    for op in ops {
        let id = op["id"].as_str().unwrap();
        assert_eq!(table[id]["operation"], op, "{id}");
        for key in [
            "args",
            if op["kind"] == "page" {
                "item"
            } else {
                "result"
            },
        ] {
            schema::check_schema(&op[key]).unwrap();
        }
        if op["scope"] == "call" {
            assert_eq!(op["bound"], json!({"task": "call"}));
            for field in ["task", "taskId", "session", "session_id", "sessionId"] {
                assert!(op["args"]["fields"].get(field).is_none());
            }
        }
        if id == "bash_status" {
            assert_eq!(op["args"], json!({"type":"record", "fields":{}}));
        }
        if id == "bash_output" {
            assert_eq!(op["page"]["total"], false);
            assert!(vectors[id].get("total").is_none());
            for item in vectors[id]["items"].as_array().unwrap() {
                schema::validate(&op["item"], item).unwrap();
            }
        } else {
            schema::validate(&op["result"], &vectors[id]).unwrap();
        }
    }
}

#[test]
fn projection_drops_unknown_fields_but_never_defaults_required_fields() {
    for op in operations::operations()
        .into_iter()
        .filter(|op| op["kind"] == "query")
    {
        let id = op["id"].as_str().unwrap();
        let mut source = fixture("result-vectors.json")[id].clone();
        source["canary"] = json!("must not leave");
        let projected = operations::project(&op["result"], &source);
        assert!(projected.get("canary").is_none(), "{id}");
        schema::validate(&op["result"], &projected).unwrap();
        let missing = operations::project(&op["result"], &json!({}));
        assert!(schema::validate(&op["result"], &missing).is_err(), "{id}");
    }
}

#[test]
fn production_is_unpublished_and_fixed_messages_are_bounded() {
    assert!(production_manifest().is_none());
    for (_, message) in operations::FIXED_MESSAGES {
        assert!(message.len() <= 1024);
    }
    assert!(operations::GENERIC_MESSAGE.len() <= 1024);
    assert_eq!(
        operations::fixed_message("unlisted_code"),
        operations::GENERIC_MESSAGE
    );
    for (code, message) in operations::FIXED_MESSAGES {
        assert_eq!(operations::fixed_message(code), *message);
    }
}

#[test]
fn builder_enforces_count_caps_at_exact_boundaries() {
    for (kind, limit, error) in [
        ("operations", 64, BuildError::Operations),
        ("views", 64, BuildError::Views),
        ("templates", 128, BuildError::Templates),
        ("pins", 16, BuildError::DefinitionDigests),
    ] {
        for extra in [0, 1] {
            let (catalog, mut ops, mut views, mut templates) = inputs();
            match kind {
                "operations" => {
                    let base = ops[0].clone();
                    ops = (0..limit + extra)
                        .map(|n| {
                            let mut v = base.clone();
                            v["id"] = json!(format!("op_{n}"));
                            v
                        })
                        .collect();
                }
                "views" => {
                    let base = views[0].clone();
                    views = (0..limit + extra)
                        .map(|n| {
                            let mut v = base.clone();
                            v["tool"] = json!(format!("tool_{n}"));
                            v
                        })
                        .collect();
                }
                "templates" => {
                    templates = (0..limit + extra)
                        .map(|n| {
                            let mut v = fixture("placeholder-row.json");
                            v["source"] = json!(format!("placeholder {n}"));
                            template(v).unwrap()
                        })
                        .collect();
                }
                "pins" => {
                    views[0]["definitionDigests"] = json!((0..limit + extra)
                        .map(|n| format!("tp1:bash:{n}:2"))
                        .collect::<Vec<_>>())
                }
                _ => unreachable!(),
            }
            let result = build(catalog, ops, views, templates);
            if extra == 0 {
                assert!(result.is_ok(), "{kind}: {:?}", result.err());
            } else {
                assert_eq!(result.err(), Some(error.clone()), "{kind}");
            }
        }
    }
}

#[test]
fn builder_enforces_source_and_manifest_byte_caps() {
    for extra in [0, 1] {
        let mut value = fixture("placeholder-row.json");
        value["source"] = json!("x".repeat(65_536 + extra));
        let result = template(value);
        if extra == 0 {
            assert!(result.is_ok());
        } else {
            assert_eq!(result.err(), Some(BuildError::SourceBytes));
        }
    }
    let (catalog, mut ops, views, templates) = inputs();
    ops[0]["result"]["fields"]["padding"] =
        json!({"type":"string", "maxBytes": 1048576, "enum":[""]});
    let base = build(catalog.clone(), ops.clone(), views.clone(), templates)
        .unwrap()
        .bytes
        .len();
    for extra in [0, 1] {
        ops[0]["result"]["fields"]["padding"]["enum"][0] =
            json!("x".repeat(MANIFEST_BYTES - base + extra));
        let templates = inputs().3;
        let result = build(catalog.clone(), ops.clone(), views.clone(), templates);
        if extra == 0 {
            assert_eq!(result.unwrap().bytes.len(), MANIFEST_BYTES);
        } else {
            assert_eq!(result.err(), Some(BuildError::ManifestBytes));
        }
    }
}

#[test]
fn builder_rejects_non_integer_numbers_in_manifest_and_templates() {
    for number in [
        json!(0.5),
        json!(9007199254740992_u64),
        json!(-9007199254740992_i64),
    ] {
        let (mut catalog, ops, views, templates) = inputs();
        catalog["version"] = number.clone();
        assert_eq!(
            build(catalog, ops, views, templates).err(),
            Some(BuildError::Number)
        );
        let mut value = fixture("placeholder-row.json");
        value["catalog"]["version"] = number;
        assert_eq!(template(value).err(), Some(BuildError::Number));
    }
}

#[test]
fn builder_rejects_unknown_or_deep_schema_constructs() {
    for bad in [
        json!({"type":"union"}),
        (0..8).fold(
            json!({"type":"boolean"}),
            |inner, _| json!({"type":"list", "items": inner, "maxItems": 1}),
        ),
    ] {
        for key in ["args", "result", "item"] {
            let (catalog, mut ops, views, templates) = inputs();
            let op = ops
                .iter_mut()
                .find(|op| (op["kind"] == "page") == (key == "item"))
                .unwrap();
            op[key] = bad.clone();
            assert_eq!(
                build(catalog, ops, views, templates).err(),
                Some(BuildError::Schema)
            );
        }
    }
}

#[test]
fn handler_projection_vectors_match_the_frozen_results() {
    let sources = fixture("handler-replies.json");
    let expected = fixture("result-vectors.json");
    for op in operations::operations()
        .iter()
        .filter(|op| op["kind"] == "query")
    {
        let id = op["id"].as_str().unwrap();
        let result = operations::project_reply(op, &sources[id]);
        assert_eq!(result, expected[id], "{id}");
        schema::validate(&op["result"], &result).unwrap();
        let result = operations::project_reply(op, &json!({}));
        assert!(schema::validate(&op["result"], &result).is_err(), "{id}");
    }
    let status = operations::operations()
        .into_iter()
        .find(|op| op["id"] == "bash_status")
        .unwrap();
    for (raw, running) in [
        ("starting", true),
        ("running", true),
        ("killing", true),
        ("completed", false),
        ("failed", false),
        ("killed", false),
        ("timed_out", false),
        ("fate_unknown", false),
    ] {
        assert_eq!(
            operations::project_reply(&status, &json!({"status":raw,"exit_code":null})),
            json!({"running":running,"exitCode":null})
        );
    }
    let bad =
        operations::project_reply(&status, &json!({"status":"future_state","exit_code":null}));
    assert!(schema::validate(&status["result"], &bad).is_err());
}

#[test]
fn field_table_covers_every_published_result_field_and_flag() {
    fn rows(schema: &Value, prefix: &str, placement: &str, out: &mut Vec<Value>) {
        for (key, field) in schema["fields"].as_object().unwrap() {
            let key = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            out.push(json!({"key":key, "required":field["optional"] != true,
                "nullable":field["nullable"] == true, "placement":placement}));
            if field["type"] == "record" {
                rows(field, &key, placement, out);
            }
        }
    }
    let table = fixture("field-table.json");
    for op in operations::operations() {
        let id = op["id"].as_str().unwrap();
        let page = op["kind"] == "page";
        let mut expected = Vec::new();
        rows(
            &op[if page { "item" } else { "result" }],
            "",
            if page { "items" } else { "result" },
            &mut expected,
        );
        if page {
            expected.push(json!({"key":"next","required":true,"nullable":true,"placement":"next"}));
        }
        let mut actual = table[id]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| {
                assert!(!field["source"].as_str().unwrap().is_empty());
                let mut field = field.clone();
                field.as_object_mut().unwrap().remove("source");
                field
            })
            .collect::<Vec<_>>();
        actual.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
        expected.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
        assert_eq!(actual, expected, "{id}");
    }
}

#[test]
fn gate_targets_bound_task_and_media_have_frozen_fixtures() {
    let gate = fixture("gate-contract.json");
    assert_eq!(
        gate["served"],
        json!([
            "read",
            "aft_outline",
            "aft_zoom",
            "aft_search",
            "grep",
            "glob",
            "aft_callgraph",
            "bash_status",
            "bash_watch"
        ])
    );
    assert_eq!(
        gate["precedence"],
        json!(["tool name", "target", "rich_ui_op"])
    );
    assert_eq!(
        gate["refusal_message"],
        "This route is read-only: <tool> is not available to the rich-UI provider."
    );
    assert_eq!(
        gate["target_refusal_message"],
        "This route is read-only: <kind> targets are not available to the rich-UI provider."
    );
    let targets = fixture("target-arguments.json");
    assert_eq!(targets.as_array().unwrap().len(), 39);
    for op in operations::operations()
        .iter()
        .filter(|op| op["scope"] == "session")
    {
        let id = op["id"].as_str().unwrap();
        for field in op["args"]["fields"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| matches!(key.as_str(), "path" | "target" | "toPath" | "batch"))
        {
            let target_field = if field == "batch" {
                "batch[].target"
            } else {
                field
            };
            assert!(targets
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["operation"] == id && t["field"] == target_field));
        }
    }
    for row in targets.as_array().unwrap() {
        let id = row["operation"].as_str().unwrap();
        let form = row["form"].as_str().unwrap();
        let gate_refusal = row["field"] != "ref"
            && (matches!(id, "outline" | "zoom")
                || (id == "read" && (form.starts_with("issue://") || form.starts_with("pr://"))));
        assert_eq!(
            row["refused_by"],
            if gate_refusal { "gate" } else { "step5" }
        );
        assert_eq!(
            row["code"],
            if gate_refusal {
                "carrier_read_only"
            } else {
                "args_invalid"
            }
        );
    }
    let bound = fixture("bound-task.json");
    assert_eq!(bound["bound"], json!({"task":"call"}));
    assert_eq!(
        bound["operations"],
        json!(["bash_status", "bash_output", "bash_watch"])
    );
    assert_eq!(bound["wire_argument"], "task");
    let media = fixture("media-outcomes.json");
    assert_eq!(media.as_array().unwrap().len(), 9);
    for row in media.as_array().unwrap() {
        assert_eq!(
            operations::media_refusal(
                row["operation"].as_str().unwrap(),
                row["kind"].as_str().unwrap()
            ),
            row["code"].as_str()
        );
    }
    assert_eq!(operations::media_refusal("read", "text"), None);
}

#[test]
fn transport_contract_pins_chunk_carrier_frame_key_and_stamp_binding() {
    let contract = fixture("transport-contract.json");
    assert_eq!(contract["chunks"]["decodedBytesMax"], CHUNK_BYTES);
    assert_eq!(
        contract["chunks"]["replyFields"],
        json!(["digest", "index", "chunks", "bytes", "data"])
    );
    assert_eq!(contract["chunks"]["refusalFrame"], "Error");
    assert_eq!(
        contract["carrier"]["bodyFields"],
        json!(["content", "isError", "structuredContent"])
    );
    assert_eq!(
        contract["carrier"]["success"],
        json!({"content":[{"type":"text","text":""}],"isError":false,"structuredContent":{"running":false,"exitCode":0}})
    );
    assert_eq!(
        contract["carrier"]["failure"]["structuredContent"]["message"],
        operations::fixed_message("args_invalid")
    );
    assert_eq!(contract["frameKey"]["freshPerReadOrAbsent"], true);
    assert_eq!(contract["frameKey"]["ledgerScope"], Value::Null);
    assert_eq!(
        contract["binding"]["equalStamp"],
        json!(["owner", "ref", "epoch"])
    );
    assert_eq!(
        contract["binding"]["noPolicyRouteCode"],
        "session_not_bound"
    );
    assert!(contract["binding"].get("equalLabels").is_none());
    assert_eq!(contract["binding"]["projectCheck"], "Check the policy-route project against the stamp workdir/project_id under scope-workdir/v1.");
}

#[test]
fn terminal_view_record_schema_vectors_and_provenance_are_frozen() {
    use crate::bash_background::BgTaskStatus;
    let record = fixture("view-record.json");
    assert_eq!(record["schema"], operations::view_record_schema());
    schema::check_schema(&record["schema"]).unwrap();
    assert_eq!(
        record["identity"],
        manifest().value["views"][0]["viewRecord"]
    );
    let fields = record["schema"]["fields"].as_object().unwrap();
    assert_eq!(record["fields"].as_array().unwrap().len(), fields.len() + 2);
    for (key, field) in fields {
        let row = record["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["key"] == key.as_str())
            .unwrap();
        assert_eq!(row["required"], true);
        assert_eq!(row["nullable"], field["nullable"] == true);
        assert!(!row["source"].as_str().unwrap().is_empty());
    }
    let mut terminal_codes = Vec::new();
    for status in [
        BgTaskStatus::Starting,
        BgTaskStatus::Running,
        BgTaskStatus::Killing,
        BgTaskStatus::Completed,
        BgTaskStatus::Failed,
        BgTaskStatus::Killed,
        BgTaskStatus::TimedOut,
        BgTaskStatus::FateUnknown,
    ] {
        if status.is_terminal() {
            terminal_codes.push(serde_json::to_value(status).unwrap());
        }
    }
    assert_eq!(
        record["schema"]["fields"]["endReason"]["enum"],
        Value::Array(terminal_codes)
    );
    for vector in record["vectors"].as_array().unwrap() {
        schema::validate(&record["schema"], vector).unwrap();
        operations::validate_view_record(vector, true).unwrap();
        assert_eq!(
            vector["templates"],
            json!({"row":manifest().value["views"][0]["row"]["template"], "page":manifest().value["views"][0]["page"]["template"]})
        );
        assert_eq!(vector["phase"], "finished");
        for key in ["task_id", "schema_pin", "endReason"] {
            assert!(!vector[key].is_null());
            let mut invalid = vector.clone();
            invalid[key] = Value::Null;
            assert_eq!(
                operations::validate_view_record(&invalid, true),
                Err("result_schema_mismatch")
            );
        }
        for (field, wrong) in [
            ("phase", json!("running")),
            ("durationMs", json!(-1)),
            ("outputBytes", json!(-1)),
            ("endReason", json!("memory_cap")),
            ("mode", json!("background")),
            ("output", json!("must not leave")),
        ] {
            let mut invalid = vector.clone();
            invalid[field] = wrong;
            assert!(
                operations::validate_view_record(&invalid, true).is_err(),
                "{field}"
            );
        }
    }
    let running = &record["hostRunningVector"];
    schema::validate(&record["schema"], running).unwrap();
    operations::validate_view_record(running, false).unwrap();
    assert_eq!(
        operations::validate_view_record(running, true),
        Err("result_schema_mismatch")
    );
    for key in [
        "task_id",
        "schema_pin",
        "endReason",
        "exitCode",
        "durationMs",
        "outputBytes",
    ] {
        assert!(running[key].is_null());
        let mut invalid = running.clone();
        invalid[key] = record["vectors"][0][key].clone();
        assert_eq!(
            operations::validate_view_record(&invalid, false),
            Err("result_schema_mismatch")
        );
    }
}

#[test]
fn bash_input_fields_match_every_pinned_definition() {
    let inputs = fixture("bash-inputs.json");
    assert_eq!(inputs["variants"].as_array().unwrap().len(), 4);
    for variant in inputs["variants"].as_array().unwrap() {
        let preset = if variant["preset"] == "head" {
            CatalogPreset::Head
        } else {
            CatalogPreset::Worker
        };
        let tool = tools_for_session(
            preset,
            &[],
            true,
            variant["runon"].as_bool().unwrap(),
            false,
        )
        .into_iter()
        .find(|tool| tool.name == "bash")
        .unwrap();
        let required = tool.input_schema["required"].as_array().unwrap();
        for key in inputs["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|key| key.as_str().unwrap())
        {
            let contract = &variant[if key == "runon" { "runonField" } else { key }];
            let actual = tool.input_schema["properties"].get(key);
            assert_eq!(
                contract["present"],
                actual.is_some(),
                "{} runon={} {key}",
                variant["preset"],
                variant["runon"]
            );
            if let Some(actual) = actual {
                assert_eq!(contract["type"], actual["type"], "{key}");
                assert_eq!(
                    contract["required"],
                    required.iter().any(|value| value == key),
                    "{key}"
                );
            }
        }
    }
}

#[test]
fn bash_output_line_items_cursors_and_retention_vectors_are_frozen() {
    let contract = fixture("bash-output.json");
    let op = operations::operations()
        .into_iter()
        .find(|op| op["id"] == "bash_output")
        .unwrap();
    assert_eq!(op["item"]["fields"]["continued"], json!({"type":"boolean"}));
    assert_eq!(
        op["item"]["fields"]["prefixDropped"],
        json!({"type":"boolean"})
    );
    assert_eq!(
        contract["retention"]["perStreamBytes"],
        crate::bash_background::buffer::DISK_LIMIT_BYTES
    );
    for vector in contract["vectors"].as_array().unwrap() {
        let page = &vector["page"];
        assert!(page.get("total").is_none());
        assert_eq!(page["next"].is_null(), vector["terminal"] == true);
        for item in page["items"].as_array().unwrap() {
            schema::validate(&op["item"], item).unwrap();
            assert!(!item["text"].as_str().unwrap().contains('\n'));
            if item["prefixDropped"] == false {
                assert_eq!(item["firstAvailableOffset"], 0);
            } else {
                assert!(
                    item["firstAvailableOffset"].is_null()
                        || item["firstAvailableOffset"].as_u64().unwrap() > 0
                );
            }
        }
        if vector["terminal"] == true {
            for (stream, raw) in vector["raw"].as_object().unwrap() {
                let consumed: u64 = page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| item["stream"] == stream.as_str())
                    .map(|item| item["bytes"].as_u64().unwrap())
                    .sum();
                assert_eq!(
                    consumed,
                    raw.as_array().unwrap().len() as u64,
                    "{} {stream}",
                    vector["name"]
                );
            }
        }
    }
    let legacy = &contract["retention"]["legacyTrim"];
    assert_eq!(
        legacy,
        &json!({"firstAvailableOffset":null,"prefixDropped":true})
    );
}

#[test]
fn shell_projection_keeps_wide_windows_remote_exit_codes() {
    for op in operations::operations()
        .iter()
        .filter(|op| matches!(op["id"].as_str(), Some("bash_status" | "bash_watch")))
    {
        let mut source = fixture("handler-replies.json")[op["id"].as_str().unwrap()].clone();
        source["status"] = json!("completed");
        source["exit_code"] = json!(4294967295_i64);
        let result = operations::project_reply(op, &source);
        assert_eq!(result["exitCode"], 4294967295_i64);
        schema::validate(&op["result"], &result).unwrap();
    }
}

#[test]
fn view_template_digests_are_exact_lowercase_file_sha256_values() {
    let record = fixture("view-record.json");
    let valid = record["vectors"][0]["templates"]["row"].as_str().unwrap();
    assert_eq!(valid.len(), 71);
    assert!(operations::valid_template_digest(valid));
    let bad = [
        String::new(),
        format!("sha256:{}", "a".repeat(63)),
        format!("sha256:{}", "a".repeat(65)),
        format!("sha256:{}", "A".repeat(64)),
        format!("sha256:{}", "g".repeat(64)),
        format!("SHA256:{}", "a".repeat(64)),
        format!("sha256:{}é", "a".repeat(62)),
    ];
    for digest in bad {
        assert!(!operations::valid_template_digest(&digest), "{digest}");
        for key in ["row", "page"] {
            let mut value = record["vectors"][0].clone();
            value["templates"][key] = json!(digest);
            assert_eq!(
                operations::validate_view_record(&value, true),
                Err("result_schema_mismatch")
            );
        }
    }
}

#[test]
fn output_cursor_contract_increments_on_no_progress_and_requires_task_binding() {
    let contract = fixture("bash-output.json");
    assert_eq!(contract["cursorContract"]["opaque"], true);
    assert_eq!(
        contract["cursorContract"]["refusalCode"],
        "malformed_cursor"
    );
    assert_eq!(
        contract["cursorContract"]["fixedMessage"],
        operations::fixed_message("malformed_cursor")
    );
    for vector in contract["cursorVectors"].as_array().unwrap() {
        if vector.get("refusal").is_some() {
            assert_eq!(vector["refusal"], "malformed_cursor");
            continue;
        }
        let request = &vector["request"];
        let response = &vector["response"];
        if vector["name"] == "terminal-drained" {
            assert!(response["next"].is_null());
            continue;
        }
        assert_ne!(request["cursor"], response["next"]);
        assert_eq!(
            response["readSequence"].as_u64().unwrap(),
            request["readSequence"].as_u64().unwrap() + 1
        );
        if vector["name"] == "no-progress" {
            assert_eq!(request["positions"], response["positions"]);
        } else {
            assert!(
                response["positions"]["stdout"].as_u64().unwrap()
                    > request["positions"]["stdout"].as_u64().unwrap()
            );
        }
    }
}

async fn real_watch_reply(
    pattern: Option<&str>,
    expired_budget: bool,
) -> crate::protocol::Response {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::bash_background::persistence::{create_task_layout, write_task_at, PersistedTask};
    use crate::bash_background::BgTaskStatus;
    use crate::commands::blocking_wait::ReplyBudget;
    use crate::config::Config;
    use crate::context::AppContext;
    use crate::harness::Harness;
    use crate::protocol::RawRequest;
    use crate::response_finalize::{DeferredResponseWake, DispatchOutcome};
    use crate::sandbox_spawn::{
        with_authenticated_principal, AuthenticatedPrincipal, PrincipalTrust,
    };

    const SESSION: &str = "rich-ui-watch-projection";
    const TASK: &str = "bash-0123456789abcdef";
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        Config {
            project_root: Some(project.path().into()),
            storage_dir: Some(storage.path().into()),
            harness: Some(Harness::Runner),
            experimental_bash_background: true,
            ..Config::default()
        },
    ));
    let principal = AuthenticatedPrincipal::RouteBind {
        trust: PrincipalTrust::FirstParty,
        route_channel: 41,
        route_epoch: 1,
        project_root: project.path().into(),
        harness: "runner".into(),
        session_id: SESSION.into(),
        principal_id: Some("reserved:broca".into()),
    };
    with_authenticated_principal(principal.clone(), || {
        let storage = crate::bash_background::task_storage_dir(&ctx);
        let resolved = create_task_layout(&storage, SESSION, TASK).unwrap();
        let mut task = PersistedTask::starting(
            TASK.into(),
            SESSION.into(),
            "printf ready".into(),
            project.path().into(),
            Some(project.path().into()),
            None,
            false,
            false,
        );
        task.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        write_task_at(&resolved, &task).unwrap();
        std::fs::write(&resolved.paths.stdout, b"ready\n").unwrap();
        std::fs::write(&resolved.paths.stderr, b"").unwrap();
        std::fs::write(&resolved.paths.exit, b"0\n").unwrap();
    });
    let mut request: RawRequest = serde_json::from_value(json!({
        "id":"watch-projection", "command":"bash_watch", "session_id":SESSION,
        "task_id":TASK, "timeout_ms":1000,
    }))
    .unwrap();
    if let Some(pattern) = pattern {
        request.params["pattern"] = json!(pattern);
    }
    let now = Instant::now();
    let budget = if expired_budget {
        // An expired scan deadline produces a real timeout without a live child
        // or a timing race. Its declared window remains positive for the schema.
        ReplyBudget {
            received_at: now - Duration::from_secs(1),
            deadline: now - Duration::from_millis(1),
        }
    } else {
        now.into()
    };
    let wake = DeferredResponseWake::default();
    let producer_wake = wake.clone();
    let producer_ctx = Arc::clone(&ctx);
    let reply = tokio::time::timeout(Duration::from_secs(30), async {
        let outcome = tokio::task::spawn_blocking(move || {
            let _wake = producer_wake.install();
            with_authenticated_principal(principal, || {
                crate::commands::bash_watch::handle_deferred_with_budget(
                    &request,
                    producer_ctx,
                    budget,
                )
            })
        })
        .await
        .unwrap();
        match outcome {
            DispatchOutcome::Immediate(response) => response,
            DispatchOutcome::Deferred(mut pending) => {
                wake.notified().await;
                (pending.poll)(&ctx).expect("watch reply queued before wake")
            }
        }
    })
    .await
    .expect("terminal watch must not wait for the watchdog");
    assert!(reply.success, "{reply:?}");
    reply
}

fn watch_operation() -> Value {
    operations::operations()
        .into_iter()
        .find(|op| op["id"] == "bash_watch")
        .unwrap()
}

#[tokio::test]
async fn watch_handler_matched_reason_projects_into_the_frozen_schema() {
    let reply = real_watch_reply(Some("ready"), false).await;
    assert_eq!(reply.data["waited"]["reason"], "matched");
    let op = watch_operation();
    let projected = operations::project_reply(&op, &reply.data);
    schema::validate(&op["result"], &projected).unwrap();
    assert_eq!(projected["waited"]["reason"], "match");
}

#[tokio::test]
async fn watch_handler_exited_reason_projects_into_the_frozen_schema() {
    let reply = real_watch_reply(None, false).await;
    assert_eq!(reply.data["waited"]["reason"], "exited");
    let op = watch_operation();
    let projected = operations::project_reply(&op, &reply.data);
    schema::validate(&op["result"], &projected).unwrap();
    assert_eq!(projected["waited"]["reason"], "exit");
}

#[tokio::test]
async fn watch_handler_timeout_reason_projects_into_the_frozen_schema() {
    let reply = real_watch_reply(Some("not-present"), true).await;
    assert_eq!(reply.data["waited"]["reason"], "timeout");
    let op = watch_operation();
    let projected = operations::project_reply(&op, &reply.data);
    schema::validate(&op["result"], &projected).unwrap();
    assert_eq!(projected["waited"]["reason"], "timeout");
}

#[tokio::test]
async fn watch_handler_unknown_reason_is_refused_without_copying_it() {
    let reply = real_watch_reply(None, false).await;
    let op = watch_operation();
    for unknown in [
        json!("unknown-watch-reason-canary"),
        json!("match"),
        json!("exit"),
        json!(null),
        json!(17),
    ] {
        let mut source = reply.data.clone();
        source["waited"]["reason"] = unknown;
        let projected = operations::project_reply(&op, &source);
        assert!(schema::validate(&op["result"], &projected).is_err());
        assert!(projected.pointer("/waited/reason").is_none());
        assert!(!projected
            .to_string()
            .contains("unknown-watch-reason-canary"));
    }
}
