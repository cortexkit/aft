use super::*;
use cortexkit_role_tool_display::RecordSchema;
use occipital_lang::{
    CatalogVersion, CheckContext, Decimal, HostProjection, HostValues, LayoutContext,
    MeasureRevision, Metrics, Network, Placement, Profile, TextStyle, ToolInputs,
};
use occipital_registry::{view as registry, Template};
use serde_json::json;

fn fixture_json<T: serde::de::DeserializeOwned>(relative: &str) -> T {
    let path = crate::test_support::manifest_dir()
        .join("../aft/tests/fixtures")
        .join(relative);
    let bytes = std::fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|err| panic!("decode {}: {err}", path.display()))
}

#[test]
#[ignore = "exports every served pin for occipital-check and measures its page"]
fn export_occipital_check_inputs() {
    let dir = std::path::Path::new(crate::test_support::manifest_dir())
        .join("../../target/tool-views-check");
    std::fs::create_dir_all(&dir).unwrap();
    for pin in SHIPPED
        .keys()
        .chain(VERSIONED_VIEW_PINS.iter())
        .chain(notice_view_pins())
    {
        let (input_schema, view, samples) = if let Some(view) = view(pin) {
            let tool = view.tool.as_ref().unwrap();
            let samples = tool_samples(&tool.name, &tool.input_schema, &view.record_schema);
            (tool.input_schema.clone(), json!(view), samples)
        } else {
            let view = notices::view(pin).unwrap();
            // The CLI takes a tool view. A notice has its data in input and does
            // not have a tool record or completion; use an empty record schema.
            let manifest = json!({
                "record_schema":{"name":"aft/notice/1", "revision":1, "fields":{}},
                "reads":[], "template":view.template
            });
            (
                view.notice.data_schema.clone(),
                manifest,
                notice_samples(pin),
            )
        };
        let filename = format!("{}.json", pin.replace(':', "_"));
        assert!(!samples.is_empty(), "{pin}");
        let schema = serde_json::from_value(view["record_schema"].clone()).unwrap();
        for (i, sample) in samples.iter().enumerate() {
            measure_page(
                &filename,
                i + 1,
                &view["template"]["page"],
                &schema,
                &input_schema,
                sample,
            );
        }
        let document = json!({"input_schema":input_schema,"view":view,"samples":samples});
        std::fs::write(dir.join(filename), serde_json::to_vec(&document).unwrap()).unwrap();
    }
}

fn tool_samples(name: &str, schema: &Value, record_schema: &RecordSchema) -> Vec<Value> {
    match name {
        "bash" => bash_samples(record_schema.completion_fields.contains_key("tail")),
        "edit" => {
            let page: Value =
                fixture_json("tool_views/phone-reader-tool-display-v8/01-page-with-call-keys.json");
            let path = &page["result"]["messages"][0]["parts"][1]["input"]["path"];
            let input = if schema["properties"].get("patch").is_some() {
                json!({"path":path,"patch":"1:ab|old\n1:ab|new"})
            } else {
                json!({"path":path,"edits":[{"oldString":"old","newString":"new"}]})
            };
            vec![json!({
                "input":input,
                "record":{"durationMs":1,"outcome":"held","reason":"consent"},
                "completion":{"durationMs":12,"heldMs":2000,"outcome":"success","fileCount":1,"modifiedCount":1,"createdCount":0,"deletedCount":0,"movedCount":0,"addedLines":1,"removedLines":1,"path":path},
                "detail":{"schema":"aft/files-detail/1","revision":1,"files":[{"change":"modified","path":path}],"hunks":[{"title":"Change 1 · line 1","oldStart":1,"newStart":1,"lines":[{"kind":"delete","text":"old"},{"kind":"add","text":"new"}]}]},
            })]
        }
        "write" | "apply_patch" => {
            let files: BTreeMap<String, Vec<Value>> =
                fixture_json("tool_views/write-and-apply-patch-samples.json");
            files[name].clone()
        }
        _ => unreachable!(),
    }
}

fn bash_samples(has_tail: bool) -> Vec<Value> {
    let input = json!({"command":"printf ok"});
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
        samples.push(json!({"input":input,"record":record,"output":"ok\n"}));
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
    let page: Value =
        fixture_json("tool_views/phone-reader-tool-display-v8/01-page-with-call-keys.json");
    let display: Value = fixture_json("tool_views/phone-reader-tool-display-v8/05-records.json");
    let display_record = &display["result"]["calls"]["ck_ls"];
    samples.push(json!({
        "input":page["result"]["messages"][0]["parts"][0]["input"],
        "record":display_record["record"], "completion":display_record["completion"]
    }));
    if has_tail {
        for sample in [
            fixture_json::<Value>("tool_views/phone-reader-tool-tail-v3/03-records.json"),
            fixture_json::<Value>("tool_views/phone-reader-tool-versions-v2/05-records-grown.json"),
        ] {
            samples.push(json!({"input":{"command":"cargo test"},
                "completion":sample["result"]["calls"]["ck_test"]["completion"]}));
        }
    }
    samples
}

fn notice_samples(pin: &str) -> Vec<Value> {
    let wake: Value = fixture_json(
        "notice-records/phone-reader-notice-records-aft-v2/02-wake-records-with-notices.json",
    );
    let riding: Value =
        fixture_json("notice-records/phone-reader-notice-records-aft-v2/03-notices-riding.json");
    wake["result"]["notices"]
        .as_object()
        .unwrap()
        .values()
        .chain([&riding["result"]["calls"]["ck_one"][0]["noticeRecord"]])
        .filter(|record| record["view"] == pin)
        .map(|record| json!({"input":record["data"]}))
        .collect()
}

// The CLI checks page source but only measures rows. Measure the evaluated
// page separately at the same two widths, with the sample's detail and output.
fn measure_page(
    file: &str,
    sample_index: usize,
    document: &Value,
    schema: &RecordSchema,
    input_schema: &Value,
    sample: &Value,
) {
    let template = Template::parse(&serde_json::to_vec(document).unwrap()).unwrap();
    let profile = Profile::ToolPage;
    let mut context = CheckContext::new(profile);
    context.catalog = CatalogVersion::Tool1_1;
    context.execution_dialect = template.execution_dialect;
    context.tool = Some(ToolInputs {
        input: HostProjection {
            value: sample["input"].clone(),
            schema: registry::input(input_schema),
        },
        view: HostProjection {
            value: json!({
                "call":{"phase":"answered","startedAtMs":null,"finishedAtMs":null,
                        "isError":null,"outputBytes":null,"unanswered":null},
                "record":sample["record"],"completion":sample["completion"],
                "detail":sample["detail"],"output":sample["output"]
            }),
            schema: registry::view(schema, profile),
        },
        truncated: false,
        truncated_fields: Vec::new(),
    });
    let evaluated = occipital_lang::evaluate_template(
        template.source.as_bytes(),
        &context,
        &HostValues::default(),
    )
    .unwrap_or_else(|error| panic!("{file}: page sample {sample_index}: {error:?}"));
    assert!(
        evaluated.abort.is_none(),
        "{file}: page sample {sample_index}: {:?}",
        evaluated.abort
    );
    let tree = evaluated.tree.expect("evaluated page");
    let revision = MeasureRevision {
        instance_epoch: 0,
        revision: 0,
        state_hash: "",
        tree: &tree,
        first: None,
    };
    for width in registry::SAMPLE_WIDTHS {
        let layout = LayoutContext {
            width: units(i128::from(width)),
            content_size_category: "large".into(),
            network: Network::Unknown,
            placement: Placement::Page,
        };
        let measured =
            occipital_lang::measure(&revision, &layout, &FixedMetrics).unwrap_or_else(|error| {
                panic!("{file}: page sample {sample_index}: width {width}: {error:?}")
            });
        println!(
            "{file}: page sample {sample_index}: width {width}: passed ({:?})",
            measured.height
        );
    }
}

// Match the CLI's synthetic metrics (not app fonts or points).
struct FixedMetrics;

fn units(value: i128) -> Decimal {
    Decimal::from_integer(value).unwrap()
}

impl Metrics for FixedMetrics {
    fn l(&self, _: TextStyle) -> Decimal {
        units(20)
    }

    fn t(&self, text: &str, _: TextStyle, width: Decimal, max_lines: Option<u32>) -> Decimal {
        let per_line = (width.to_string().parse::<f64>().unwrap() / 8.0)
            .floor()
            .max(1.0);
        let lines = (text.chars().count().max(1) as f64 / per_line).ceil() as i128;
        units(lines.min(i128::from(max_lines.unwrap_or(u32::MAX))) * 20)
    }

    fn tw(&self, text: &str, _: TextStyle) -> Decimal {
        units(text.chars().count() as i128 * 8)
    }
}
