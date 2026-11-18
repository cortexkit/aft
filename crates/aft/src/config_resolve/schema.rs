//! Editor schema for aft.jsonc. The raw deserializer defines the file shape;
//! the runtime `Config` contains process state that files may not set.

use crate::config_resolve::{RawAftConfig, RawEditMode, RawPiToolPresentation};
use schemars::{generate::SchemaSettings, JsonSchema, Schema, SchemaGenerator};
use serde_json::{json, Value};
use std::borrow::Cow;

macro_rules! string_schema {
    ($type:ty) => {
        impl JsonSchema for $type {
            fn schema_name() -> Cow<'static, str> {
                stringify!($type).into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                schemars::json_schema!({"type": "string"})
            }
        }
    };
}

// Both enums deliberately preserve unrecognized strings for resolver warnings.
string_schema!(RawEditMode);
string_schema!(RawPiToolPresentation);

pub(crate) fn lsp_idle_minutes(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({"anyOf": [{"type":"integer"}, {"const":"never"}, {"type":"null"}]})
}

pub fn editor_schema() -> Value {
    let mut settings = SchemaSettings::draft2020_12();
    settings.inline_subschemas = true;
    let mut schema = serde_json::to_value(
        settings
            .into_generator()
            .into_root_schema_for::<RawAftConfig>(),
    )
    .expect("serialize schema");
    let root = schema.as_object_mut().expect("AFT config object");
    root.insert("title".into(), json!("AFT configuration"));
    root.insert(
        "$id".into(),
        json!("https://raw.githubusercontent.com/cortexkit/aft/main/assets/aft.schema.json"),
    );
    root.remove("additionalProperties");
    root.insert("unevaluatedProperties".into(), json!(false));

    let fields = root["properties"]
        .as_object_mut()
        .expect("AFT config fields");
    // Annotate optional values with their runtime defaults without filling in
    // missing fields: absence and an explicit choice can merge differently.
    let bash_schema = fields.get_mut("bash").expect("bash schema");
    let bash = object_alternative(bash_schema).expect("bash object");
    let defaults = crate::config::BashConfig::default();
    for (key, default) in [
        ("linux_scope", json!(defaults.linux_scope)),
        (
            "linux_scope_max_runtime_secs",
            json!(defaults.linux_scope_max_runtime_secs),
        ),
        ("disclaim_privacy", json!(defaults.disclaim_privacy)),
    ] {
        bash["properties"][key]["default"] = default;
    }
    // The raw branches have disjoint types; flatten the optional wrapper for
    // editors that use the top-level alternatives to present completion.
    bash_schema["oneOf"] = Value::Array(flatten_alternatives(bash_schema));
    bash_schema.as_object_mut().unwrap().remove("anyOf");
    // Only the active harness is interpreted as settings. Keep the other
    // blocks opaque while showing the same suggestions as the base settings.
    let mut harness_fields = fields.clone();
    harness_fields.remove("harnesses");
    fields.get_mut("harnesses").expect("harness schema")["additionalProperties"] = json!({
        "properties": harness_fields,
    });
    schema
}

fn flatten_alternatives(schema: &Value) -> Vec<Value> {
    match schema.get("anyOf").and_then(Value::as_array) {
        Some(branches) => branches.iter().flat_map(flatten_alternatives).collect(),
        None => vec![schema.clone()],
    }
}

fn object_alternative(schema: &mut Value) -> Option<&mut Value> {
    if schema.get("properties").is_some() {
        return Some(schema);
    }
    schema
        .get_mut("anyOf")?
        .as_array_mut()?
        .iter_mut()
        .find_map(object_alternative)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[test]
    fn config_schema_keys_match_the_raw_parser() {
        struct Fields<'a>(&'a mut &'static [&'static str]);
        impl<'de> serde::Deserializer<'de> for Fields<'_> {
            type Error = serde::de::value::Error;
            fn deserialize_any<V: serde::de::Visitor<'de>>(
                self,
                _: V,
            ) -> Result<V::Value, Self::Error> {
                Err(serde::de::Error::custom("field probe"))
            }
            fn deserialize_struct<V: serde::de::Visitor<'de>>(
                self,
                _: &'static str,
                fields: &'static [&'static str],
                visitor: V,
            ) -> Result<V::Value, Self::Error> {
                *self.0 = fields;
                self.deserialize_any(visitor)
            }
            serde::forward_to_deserialize_any! { bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map enum identifier ignored_any }
        }
        let mut parser_fields = &[][..];
        assert!(RawAftConfig::deserialize(Fields(&mut parser_fields)).is_err());
        assert!(!parser_fields.is_empty());
        let schema = editor_schema();
        let schema_fields = schema["properties"].as_object().unwrap();
        assert_eq!(schema_fields.len(), parser_fields.len());
        for key in parser_fields {
            assert!(schema_fields.contains_key(*key), "missing {key}");
        }
    }

    fn annotations<'a>(
        node: &'a Value,
        path: &str,
        found: &mut BTreeMap<(String, &'a str), &'a str>,
    ) {
        for keyword in ["description", "markdownDescription"] {
            if let Some(text) = node.get(keyword).and_then(Value::as_str) {
                assert!(
                    found.insert((path.to_owned(), keyword), text).is_none(),
                    "duplicate {keyword} at {path}"
                );
            }
        }
        if let Some(properties) = node.get("properties").and_then(Value::as_object) {
            for (name, child) in properties {
                annotations(child, &format!("{path}.{name}"), found);
            }
        }
        for (keyword, suffix) in [("additionalProperties", ".*"), ("items", "[]")] {
            if let Some(child) = node.get(keyword) {
                annotations(child, &format!("{path}{suffix}"), found);
            }
        }
        for keyword in ["oneOf", "anyOf", "allOf"] {
            if let Some(branches) = node.get(keyword).and_then(Value::as_array) {
                for branch in branches {
                    // Object branches do not change the path users type. The
                    // bash shorthand, however, has its own separate hover.
                    if branch.get("type").and_then(Value::as_str) == Some("boolean") {
                        annotations(branch, &format!("{path}<boolean>"), found);
                    } else {
                        annotations(branch, path, found);
                    }
                }
            }
        }
    }

    #[test]
    fn config_schema_preserves_legacy_editor_annotations() {
        let legacy: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/config_schema/legacy_editor_schema.json"
        ))
        .unwrap();
        let generated = editor_schema();
        let (mut expected, mut actual) = (BTreeMap::new(), BTreeMap::new());
        annotations(&legacy, "", &mut expected);
        annotations(&generated, "", &mut actual);
        assert!(
            expected.len() >= 244,
            "legacy schema snapshot is incomplete"
        );
        for (key, text) in expected {
            assert_eq!(
                actual.get(&key),
                Some(&text),
                "missing or changed {} at {}",
                key.1,
                key.0
            );
        }
    }

    #[test]
    fn config_schema_suggests_every_inspect_category() {
        let schema = editor_schema();
        for path in [
            "/properties",
            "/properties/harnesses/additionalProperties/properties",
        ] {
            let categories =
                &schema.pointer(path).unwrap()["inspect"]["properties"]["categories"]["properties"];
            assert_eq!(
                categories.as_object().unwrap().len(),
                crate::config::InspectCategories::KEYS.len()
            );
            for name in crate::config::InspectCategories::KEYS {
                assert_eq!(categories[name]["type"], "boolean");
                assert_eq!(categories[name]["default"], true);
            }
        }
    }
}
