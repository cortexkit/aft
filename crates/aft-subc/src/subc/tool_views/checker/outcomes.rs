use super::*;

const WATCH_NOTICE: &str = "nv1:aft:bash_watch:1";
const LAST_LINE: &str = "last line";

const OUTCOMES: [Outcome; 3] = [
    Outcome {
        status: "killed",
        end: "killed",
        exit: Some(143),
        bash_text: "Stopped",
        notice_text: "Stopped",
        tone: "muted",
    },
    Outcome {
        status: "timed_out",
        end: "timed_out",
        exit: None,
        bash_text: "Timed out",
        notice_text: "Timed out",
        tone: "danger",
    },
    Outcome {
        status: "completed",
        end: "exited",
        exit: Some(1),
        bash_text: "Exit 1",
        notice_text: "Failed · exit 1",
        tone: "danger",
    },
];

#[derive(Clone, Copy)]
struct Outcome {
    status: &'static str,
    end: &'static str,
    exit: Option<i64>,
    bash_text: &'static str,
    notice_text: &'static str,
    tone: &'static str,
}

impl Outcome {
    fn bash_sample(self, background: bool, tail: bool) -> Value {
        let input = json!({"command":"printf 'last line'","description":"Print the last line"});
        let mut terminal = json!({"durationMs":8,"outcome":"error","phase":"foreground_finished","execution":"local","endReason":self.end});
        if let Some(code) = self.exit {
            terminal["exitCode"] = json!(code);
        }
        if background {
            terminal["phase"] = json!("background_finished");
            if tail {
                terminal["tail"] = json!([{"text":LAST_LINE,"tone":"danger"}]);
            }
            let started = json!({"durationMs":1,"outcome":"success","phase":"background_started","execution":"local","taskId":"bash-demo1"});
            json!({"input":input,"record":started,"completion":terminal})
        } else {
            json!({"input":input,"record":terminal,"output":LAST_LINE})
        }
    }

    fn notice_sample(self, kind: &str) -> Value {
        let mut input = json!({"kind":kind,"task_id":"bash-demo1","status":self.status,"command":"printf 'last line'","output_preview":LAST_LINE,"output_cut":false});
        if let Some(code) = self.exit {
            input["exit_code"] = json!(code);
        }
        if kind == "watch" {
            input["watch_reason"] = json!("task_exit");
        }
        json!({"input":input})
    }
}

pub(super) fn bash_samples(has_tail: bool) -> impl Iterator<Item = Value> {
    OUTCOMES.into_iter().flat_map(move |outcome| {
        [false, true]
            .into_iter()
            .map(move |background| outcome.bash_sample(background, has_tail))
    })
}

pub(super) fn notice_samples(pin: &str) -> impl Iterator<Item = Value> {
    let kind = match pin {
        FINISHED_NOTICE => Some("finished"),
        WATCH_NOTICE => Some("watch"),
        _ => None,
    };
    kind.into_iter().flat_map(|kind| {
        OUTCOMES
            .into_iter()
            .map(move |outcome| outcome.notice_sample(kind))
    })
}

#[test]
fn bash_tool_views_paint_stops_neutral_but_keep_errors_red() {
    let mut checked = 0;
    for pin in SHIPPED.keys().chain(VERSIONED_VIEW_PINS.iter()) {
        let view = view(pin).unwrap();
        let tool = view.tool.as_ref().unwrap();
        if tool.name != "bash" {
            continue;
        }
        let has_tail = view.record_schema.completion_fields.contains_key("tail");
        for outcome in OUTCOMES {
            for background in [false, true] {
                let (row, page) = render(
                    pin,
                    &view.template,
                    &view.record_schema,
                    &tool.input_schema,
                    &outcome.bash_sample(background, has_tail),
                );
                assert_row_failure(&row, outcome.tone == "danger");
                assert_text_tone(&page, outcome.bash_text, outcome.tone);
                if has_tail && background {
                    assert_text_tone(&page, LAST_LINE, "danger");
                }
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "no Bash view samples checked");
}

#[test]
fn bash_terminal_notices_paint_stops_neutral_but_keep_errors_red() {
    for (pin, kind) in [(FINISHED_NOTICE, "finished"), (WATCH_NOTICE, "watch")] {
        let view = notices::view(pin).unwrap();
        let manifest = notice_manifest(&view);
        let schema = serde_json::from_value(manifest["record_schema"].clone()).unwrap();
        let render_sample = |sample: &Value| {
            render(
                pin,
                &manifest["template"],
                &schema,
                &view.notice.data_schema,
                sample,
            )
        };
        for outcome in OUTCOMES {
            let (row, page) = render_sample(&outcome.notice_sample(kind));
            assert_row_failure(&row, outcome.tone == "danger");
            assert_text_tone(&page, outcome.notice_text, outcome.tone);
            if kind == "watch" {
                assert_watch_link(&page);
            }
        }
        if kind == "watch" {
            let mut sample = OUTCOMES[0].notice_sample(kind);
            sample["input"]["watch_reason"] = json!("pattern_match");
            sample["input"]["watch_match"] = json!("ready");
            let (row, page) = render_sample(&sample);
            assert_row_failure(&row, false);
            assert_text_tone(&page, "Printed “ready”", "muted");
            assert_watch_link(&page);
        }
    }
}

fn render(
    pin: &str,
    template: &Value,
    schema: &RecordSchema,
    input_schema: &Value,
    sample: &Value,
) -> (Tree, Tree) {
    let render =
        |slot: &str, profile| evaluate(pin, &template[slot], schema, input_schema, sample, profile);
    (
        render("row", Profile::ToolRow),
        render("page", Profile::ToolPage),
    )
}

fn assert_row_failure(row: &Tree, expected: bool) {
    let actual = row
        .blocks
        .iter()
        .find_map(|node| prop(node, "ToolHeader", "failed"));
    assert_eq!(actual, Some(&TreeValue::Boolean(expected)), "{row:?}");
}

fn assert_text_tone(page: &Tree, text: &str, tone: &str) {
    let actual = page.blocks.iter().find_map(|node| {
        (string_prop(node, "ToolText", "text") == Some(text))
            .then(|| string_prop(node, "ToolText", "tone"))
            .flatten()
    });
    assert_eq!(actual, Some(tone), "{text}: {page:?}");
}

fn assert_watch_link(page: &Tree) {
    let Some(TreeNode::Markdown { children, .. }) = page.blocks.last() else {
        panic!("expected the watch notice's Shells link: {page:?}");
    };
    let [link] = children.as_slice() else {
        panic!("expected one inline link, got {children:?}");
    };
    assert_eq!(
        string_prop(link, "OpenItem", "id"),
        Some("shell:bash-demo1")
    );
    assert_eq!(
        string_prop(link, "OpenItem", "label"),
        Some("Shell bash-demo1")
    );
}

fn string_prop<'a>(node: &'a TreeNode, component: &str, key: &str) -> Option<&'a str> {
    match prop(node, component, key)? {
        TreeValue::String(value) => Some(value.as_str()),
        _ => None,
    }
}

fn prop<'a>(node: &'a TreeNode, component: &str, key: &str) -> Option<&'a TreeValue> {
    let TreeNode::Component { name, props, .. } = node else {
        return None;
    };
    if name.as_str() != component {
        return None;
    }
    props
        .iter()
        .find(|(name, _)| name.as_str() == key)
        .map(|(_, value)| value)
}
