//! Permanent views keyed by literal served pins, not by today's catalog.
use std::collections::BTreeMap;
use std::sync::LazyLock;

use cortexkit_role_tool_provider::views::{ViewEntry, ViewManifest, ViewsReply, ViewsRequest};
use serde::Deserialize;
use serde_json::Value;
use subc_protocol::ErrorBody;

#[derive(Deserialize)]
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
    SHIPPED.get(pin)?.history.last().cloned()
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
            for old in kept.history {
                assert!(
                    matches!(
                        fetched.compare_with_kept(&old),
                        ViewChange::Same | ViewChange::NewerTemplate
                    ),
                    "changed frozen view: {pin}"
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
    fn placeholders_pass_occipital_check_and_have_revision_zero() {
        for (pin, entry) in SHIPPED.iter() {
            let view = view(pin).unwrap();
            assert_eq!(view.template_revision, 0);
            let reports = cortexkit_occipital_registry::view::check_view(
                &view.template,
                &view.record_schema,
                &entry.tool.input_schema,
            )
            .unwrap();
            assert_eq!(reports.len(), 2);
            for report in reports {
                assert!(report.passed(), "{pin}: {report:?}");
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
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tool-views-check");
        std::fs::create_dir_all(&dir).unwrap();
        for (index, (pin, entry)) in SHIPPED.iter().enumerate() {
            std::fs::write(dir.join(format!("{index}.json")), serde_json::to_vec(&json!({"input_schema":entry.tool.input_schema,"view":view(pin).unwrap(),"samples":[{"input":{}}]})).unwrap()).unwrap();
        }
    }
}
