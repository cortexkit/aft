//! Embedded runner guidance, checked against declared facts before rendering.

use std::sync::LazyLock;

use cortexkit_persona::{FactsShape, GuidanceSet, PersonaError, RenderLimits};
use cortexkit_role_tool_provider::{SystemTextFacts, SYSTEM_TEXT_FACTS_SCHEMA};
use serde_json::{json, Value};

use super::CatalogPreset;

const TOOL_KEYS: &[&str] = &[
    "search",
    "outline",
    "zoom",
    "callgraph",
    "inspect",
    "read",
    "bash",
    "bash_watch",
    "bash_status",
];

static GUIDANCE: LazyLock<GuidanceSet> = LazyLock::new(|| {
    checked_set([
        ("main", include_str!("guidance/main.j2")),
        ("code", include_str!("guidance/code.j2")),
        ("scope", include_str!("guidance/scope.j2")),
    ])
    .expect("embedded AFT runner guidance must use only declared facts")
});

fn checked_set(
    templates: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> Result<GuidanceSet, PersonaError> {
    let tools: serde_json::Map<String, Value> = TOOL_KEYS
        .iter()
        .map(|name| ((*name).into(), json!({"type": "string"})))
        .collect();
    let shape = FactsShape::from_json_schema(&json!({
        "type": "object",
        "properties": {
            "preset": {"type": "string"},
            "facts": serde_json::from_str::<Value>(SYSTEM_TEXT_FACTS_SCHEMA)
                .expect("published SystemTextFacts schema must be JSON"),
            "tools": {"type": "object", "properties": tools},
            "worker": {"type": "boolean"},
            "hashline_tags": {"type": "string"}
        }
    }))?;
    let set = GuidanceSet::new(templates)?;
    set.check(&shape)?;
    Ok(set)
}

fn render_facts(
    preset: CatalogPreset,
    facts: Option<&SystemTextFacts>,
    spell: &dyn Fn(&str) -> Option<String>,
    worker: bool,
    hashline_tags: Option<&str>,
) -> Value {
    // Core's plan facts remain separate from AFT's served names: a plan's tool
    // list must never make guidance name a tool this catalog does not serve.
    let tools: serde_json::Map<String, Value> = TOOL_KEYS
        .iter()
        .map(|name| ((*name).into(), json!(spell(name).unwrap_or_default())))
        .collect();
    let mut rendered = json!({
        "preset": preset.name(),
        "tools": tools,
        "worker": worker,
        "hashline_tags": hashline_tags.unwrap_or_default()
    });
    if let Some(facts) = facts {
        rendered["facts"] = serde_json::to_value(facts).expect("SystemTextFacts must serialize");
    }
    rendered
}

pub(super) fn render(
    preset: CatalogPreset,
    facts: Option<&SystemTextFacts>,
    spell: &dyn Fn(&str) -> Option<String>,
    worker: bool,
    hashline_tags: Option<&str>,
) -> String {
    GUIDANCE
        .render(
            "main",
            &render_facts(preset, facts, spell, worker, hashline_tags),
            RenderLimits::default(),
        )
        .expect("checked AFT runner guidance must render from catalog facts")
        .text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undeclared_names_are_refused_when_loading_even_in_unused_branches() {
        for source in [
            "{% if false %}{{ undeclared }}{% endif %}",
            "{% if false %}{{ facts.harness.undeclared }}{% endif %}",
            "{% if tools.missing is defined %}unused{% endif %}",
        ] {
            let error = checked_set([("main", source)]).unwrap_err().to_string();
            assert!(
                error.contains("undeclared") || error.contains("missing"),
                "{error}"
            );
        }
        assert!(checked_set([("main", "ok"), ("unused", "{{ missing }}")]).is_err());
    }

    #[test]
    fn plan_facts_and_catalog_spellings_are_distinct_render_inputs() {
        let plan = json!({
            "model": {"provider": "vendor", "id": "{{ verbatim }}\r\n"},
            "harness": {"id": "runner", "facts": ["shell"]},
            "tools": ["search"], "idle_start": []
        });
        let facts = serde_json::from_value(plan.clone()).unwrap();
        let rendered = render_facts(
            CatalogPreset::Head,
            Some(&facts),
            &|name| (name == "read").then(|| "read".into()),
            false,
            None,
        );
        assert_eq!(rendered["facts"], plan);
        assert_eq!(rendered["tools"]["search"], "");
        assert_eq!(rendered["tools"]["read"], "read");
        let set =
            checked_set([("main", "{{ preset }}:{{ facts.model.id }}:{{ tools.read }}")]).unwrap();
        assert_eq!(
            set.render("main", &rendered, RenderLimits::default())
                .unwrap()
                .text,
            "head:{{ verbatim }}\r\n:read"
        );
        assert!(
            render_facts(CatalogPreset::Reader, None, &|_| None, false, None)
                .get("facts")
                .is_none()
        );
    }
}
