//! Registered, typed notice text shared by `tool.catalog` and runner sends.

use cortexkit_role_llm_runner::delivery_order::{Slot, Template};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct PendingNotices {
    pub(crate) route: Option<crate::subc::runner_notices::CodeSearchNotice>,
    pub(crate) items: Vec<Notice>,
}

#[derive(Debug, Clone)]
pub(crate) struct Notice {
    pub(crate) text: String,
    pub(crate) template: Option<(&'static str, Value)>,
    pub(crate) target: Target,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Target {
    RewriteFooter,
    BashOutput,
    Text,
}

impl Notice {
    pub(crate) fn template(id: &'static str, slots: Value, text: String, target: Target) -> Self {
        Self {
            text,
            template: Some((id, slots)),
            target,
        }
    }

    pub(crate) fn diagnostic(text: String) -> Self {
        Self {
            text,
            template: None,
            target: Target::Text,
        }
    }
}

pub(crate) fn append_text(text: &mut String, notice: &str) {
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(notice);
}

/// Consume each decision once. Only this boundary chooses send versus append.
pub(crate) fn deliver(response: &mut crate::protocol::Response, mut text: Option<&mut String>) {
    for notice in std::mem::take(&mut response.notices.items) {
        if let Some(route) = &response.notices.route {
            route.send(&notice);
            continue;
        }
        match notice.target {
            Target::RewriteFooter => {
                crate::bash_rewrite::rules::append_decided_footer(response, &notice.text)
            }
            Target::BashOutput => {
                crate::bash_rewrite::bash_hints::append_nudge(response, &notice.text)
            }
            Target::Text => {
                if let Some(text) = text.as_deref_mut() {
                    append_text(text, &notice.text);
                } else if let Some(existing) = response.data.get_mut("text") {
                    if let Some(value) = existing.as_str() {
                        let mut value = value.to_owned();
                        append_text(&mut value, &notice.text);
                        *existing = Value::String(value);
                    }
                }
            }
        }
    }
}

pub(crate) const CODE_SEARCH: &str = "aft.bash.code_search";
pub(crate) const PREFER_TOOL: &str = "aft.bash.prefer_tool";
pub(crate) const IDENTICAL: &str = "aft.repeat_call.identical";
pub(crate) const DRIFTING: &str = "aft.repeat_call.drifting";

const SEARCH_REDIRECT: &str = "Use the `aft_search` tool instead (it auto-routes concepts, identifiers, regex, and literals).";
const GREP_REDIRECT: &str = "Use the `grep` tool instead (indexed and ranked).";
const WORKER_SEARCH_REDIRECT: &str =
    "Use the `search` tool instead (it auto-routes concepts, identifiers, regex, and literals).";

fn enumeration(values: &[&str]) -> Slot {
    Slot::Enum {
        values: values.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn shell_slot() -> Slot {
    enumeration(&["bash", "shell"])
}

fn bash_templates(worker: bool) -> Vec<Template> {
    vec![
        Template::new(
            CODE_SEARCH,
            "DO NOT search code by running grep/rg in {shell} — it is unindexed, unranked, and serial. {redirect}",
            BTreeMap::from([
                ("shell".into(), shell_slot()),
                ("redirect".into(), enumeration(&[if worker { WORKER_SEARCH_REDIRECT } else { SEARCH_REDIRECT }, GREP_REDIRECT])),
            ]),
        ),
        Template::new(
            PREFER_TOOL,
            "Prefer `{tool}` tool over {shell}.",
            BTreeMap::from([
                ("tool".into(), enumeration(&["glob", "read", "edit"])),
                ("shell".into(), shell_slot()),
            ]),
        ),
    ]
}

pub(crate) fn code_search_slots(search_tool: Option<&str>, shell: &str) -> Value {
    json!({"shell": shell, "redirect": match search_tool {
        Some("search") => WORKER_SEARCH_REDIRECT,
        Some(_) => SEARCH_REDIRECT,
        None => GREP_REDIRECT,
    }})
}

pub(crate) fn templates(preset: &str) -> Vec<Template> {
    let mut templates = if preset == "reader" {
        vec![]
    } else {
        bash_templates(preset == "worker")
    };
    let mut instructions = vec![];
    for worker in [false, true] {
        if worker && preset == "reader" {
            continue;
        }
        for watch in if worker {
            vec![true, false]
        } else {
            vec![false]
        } {
            for escalated in [false, true] {
                instructions.push(crate::response_finalize::repeat_instruction(
                    worker, watch, escalated,
                ));
            }
        }
    }
    for (id, text) in [
        (IDENTICAL, "This call ran {count} times with the same arguments and the same output in {span} s. It is not returning anything new. {instruction}"),
        (DRIFTING, "This call ran {count} times with the same arguments in {span} s, and only its output drifts (for example, a timestamp). {instruction}"),
    ] {
        templates.push(Template::new(id, text, BTreeMap::from([
            ("count".into(), Slot::Integer { min: 2, max: 1000 }),
            ("span".into(), Slot::Integer { min: 0, max: 86400 }),
            ("instruction".into(), enumeration(&instructions)),
        ])));
    }
    templates
}
