use serde_json::{json, Value};

use super::{catalog, CatalogPreset};

fn request(preset: CatalogPreset) -> Value {
    let mut request = json!({
        "preset": preset.name(), "system_text": {"preset": preset.name()}
    });
    if preset == CatalogPreset::Head {
        request["params"] = json!({"model": {"provider": "vendor", "model": "gpt-5"}});
    }
    request
}

#[test]
fn approved_text_for_every_preset_with_full_and_reduced_tool_sets() {
    for (preset, full, reduced) in [
        (
            CatalogPreset::Head,
            r"# Agent Foundational Toolkit

**Finding code**: start with aft_search; it takes concepts, identifiers, regex and literals. Then aft_outline for structure, aft_zoom for named symbols, read for line ranges. Don't find or read code with grep, find, sed or cat in bash.
**Relationships**: aft_callgraph for callers, impact, and execution or data-flow traces.
**Web pages**: aft_outline with a URL as target, then aft_zoom on a heading.
**Parallel calls**: send independent read-only calls in one response.
**After edits**: run aft_inspect for diagnostics; the project's typecheck and tests are the final check.
**Status bar**: `[AFT E.. W.. | T..]` appears when a count changes. E above 0 usually means your last edit broke something; `?` means unknown, not zero.
**Commands**: don't pipe test or build output into tail, grep or head; AFT already shortens it. Pass wait: true when you need a long command's result before anything else. Otherwise a long command moves to the background and a notice brings its result. Block on it with bash_watch only when nothing else is left, and never loop bash_status.
",
            r"# Agent Foundational Toolkit

**Finding code**: start with aft_search; it takes concepts, identifiers, regex and literals. Don't find or read code with grep, find, sed or cat in bash.
**Parallel calls**: send independent read-only calls in one response.
**After edits**: run aft_inspect for diagnostics; the project's typecheck and tests are the final check.
**Status bar**: `[AFT E.. W.. | T..]` appears when a count changes. E above 0 usually means your last edit broke something; `?` means unknown, not zero.
**Commands**: don't pipe test or build output into tail, grep or head; AFT already shortens it. Pass wait: true when you need a long command's result before anything else. Otherwise a long command moves to the background and a notice brings its result.
",
        ),
        (
            CatalogPreset::Worker,
            r"# Agent Foundational Toolkit

**Finding code**: start with search; it takes concepts, identifiers, regex and literals. Then outline for structure, zoom for named symbols, read for line ranges. Don't find or read code with grep, find, sed or cat in bash.
**Relationships**: callgraph for callers, impact, and execution or data-flow traces.
**Web pages**: outline with a URL as target, then zoom on a heading.
**Parallel calls**: send independent read-only calls in one response.
**After edits**: run inspect for diagnostics; the project's typecheck and tests are the final check.
**Status bar**: `[AFT E.. W.. | T..]` appears when a count changes. E above 0 usually means your last edit broke something; `?` means unknown, not zero.
**Commands**: don't pipe test or build output into tail, grep or head; AFT already shortens it. Pass wait: true to wait for a long command; after the worker wait limit (bash.worker_wait_max_ms, 30 minutes by default) it reports the command is still running. A background command never wakes you: wait on it with bash_watch before you report, and never loop bash_status.
As a worker, keep changes within your assigned scope, verify them, and report the result to your parent.
",
            r"# Agent Foundational Toolkit

**Finding code**: start with search; it takes concepts, identifiers, regex and literals. Don't find or read code with grep, find, sed or cat in bash.
**Parallel calls**: send independent read-only calls in one response.
**After edits**: run inspect for diagnostics; the project's typecheck and tests are the final check.
**Status bar**: `[AFT E.. W.. | T..]` appears when a count changes. E above 0 usually means your last edit broke something; `?` means unknown, not zero.
**Commands**: don't pipe test or build output into tail, grep or head; AFT already shortens it. Pass wait: true to wait for a long command; after the worker wait limit (bash.worker_wait_max_ms, 30 minutes by default) it reports the command is still running.
As a worker, keep changes within your assigned scope, verify them, and report the result to your parent.
",
        ),
        (
            CatalogPreset::Reader,
            r"# Agent Foundational Toolkit

This session is read-only: it can read and search code, but it has no tool that runs commands or changes, moves or deletes files.
**Finding code**: start with search; it takes concepts, identifiers, regex and literals. Then outline for structure, zoom for named symbols, read for line ranges.
**Relationships**: callgraph for callers, impact, and execution or data-flow traces.
**Web pages**: outline with a URL as target, then zoom on a heading.
**Parallel calls**: send independent read-only calls in one response.
Use inspect for diagnostics.
",
            r"# Agent Foundational Toolkit

This session is read-only: it can read and search code, but it has no tool that runs commands or changes, moves or deletes files.
**Finding code**: start with search; it takes concepts, identifiers, regex and literals.
**Parallel calls**: send independent read-only calls in one response.
Use inspect for diagnostics.
",
        ),
    ] {
        for (disabled, expected) in [
            (vec![], full),
            (
                [
                    "read",
                    "aft_zoom",
                    "aft_callgraph",
                    "bash_watch",
                    "bash_status",
                ]
                .map(String::from)
                .to_vec(),
                reduced,
            ),
        ] {
            let reply = catalog(request(preset), &disabled, true).unwrap();
            assert_eq!(
                reply["system_text"]["text"],
                expected,
                "{} {disabled:?}",
                preset.name()
            );
        }
    }
}

#[test]
fn sentences_require_every_named_tool_without_fallbacks() {
    for preset in CatalogPreset::ALL {
        for disabled in [
            "aft_search",
            "aft_outline",
            "aft_zoom",
            "read",
            "aft_callgraph",
            "aft_inspect",
            "bash",
            "bash_watch",
            "bash_status",
        ] {
            let reply = catalog(request(preset), &[disabled.into()], true).unwrap();
            let text = reply["system_text"]["text"].as_str().unwrap();
            let serves = |bare: &str| {
                reply["tools"].as_array().unwrap().iter().any(|tool| {
                    let name = tool["name"].as_str().unwrap();
                    name == bare || super::same_tool(name, bare)
                })
            };
            for (phrase, required) in [
                ("start with ", &["search"][..]),
                ("for line ranges.", &["outline", "zoom", "read"][..]),
                ("**Relationships**:", &["callgraph"][..]),
                ("**Web pages**:", &["outline", "zoom"][..]),
                ("for diagnostics", &["inspect"][..]),
                (
                    "never loop bash_status",
                    &["bash", "bash_watch", "bash_status"][..],
                ),
            ] {
                assert_eq!(
                    text.contains(phrase),
                    required.iter().all(|tool| serves(tool)),
                    "{} -{disabled} {phrase}: {text}",
                    preset.name()
                );
            }
            assert_eq!(text.contains("**Commands**:"), serves("bash"));
            assert_eq!(text.contains("Don't find or read code"), serves("bash"));
            assert!(!text.contains("Use read for files"));
        }
    }
}
