//! Registered, typed notice text shared by `tool.catalog` and runner sends.

#[cfg(test)]
pub(crate) use crate::bash_rewrite::notices::PREFER_TOOL;
pub(crate) use crate::bash_rewrite::notices::{
    append_text, code_search_slots, deliver, Notice, Target, CODE_SEARCH,
};
use crate::bash_rewrite::notices::{bash_templates, enumeration};
use cortexkit_role_llm_runner::delivery_order::{Slot, Template};
use std::collections::BTreeMap;

pub(crate) const IDENTICAL: &str = "aft.repeat_call.identical";
pub(crate) const DRIFTING: &str = "aft.repeat_call.drifting";

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
