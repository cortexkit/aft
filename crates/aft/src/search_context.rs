use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::fmt;

const MAX_CHARACTERS: usize = 6000;

#[derive(Clone)]
pub struct SearchContext(String);

impl SearchContext {
    pub fn new(text: &str) -> Self {
        let start = text
            .char_indices()
            .rev()
            .nth(MAX_CHARACTERS - 1)
            .map_or(0, |(index, _)| index);
        Self(text[start..].to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_request_field(value: Option<&Value>) -> Result<Option<Self>, &'static str> {
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(Self::new(text))),
            Some(value) => {
                let kind = match value {
                    Value::Bool(_) => "boolean",
                    Value::Number(_) => "number",
                    Value::Array(_) => "array",
                    Value::Object(_) => "object",
                    Value::Null => "null",
                    Value::String(_) => "string",
                };
                log::warn!("search_context ignored: type={kind}");
                Ok(None)
            }
        }
    }
}

impl fmt::Debug for SearchContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchContext")
            .field("characters", &self.0.chars().count())
            .finish()
    }
}

pub(crate) fn deserialize_optional<'de, D>(
    deserializer: D,
) -> Result<Option<SearchContext>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    SearchContext::from_request_field(Some(&value)).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gather_context_caps_newest_unicode_characters() {
        let text = format!("discard{}🙂", "界".repeat(5999));
        let context = SearchContext::new(&text);
        assert_eq!(context.as_str(), format!("{}🙂", "界".repeat(5999)));
        assert_eq!(context.as_str().chars().count(), 6000);
        assert_eq!(SearchContext::new("short🙂").as_str(), "short🙂");
        assert_eq!(SearchContext::new("").as_str(), "");
    }

    #[test]
    fn gather_context_debug_never_contains_text() {
        let debug = format!("{:?}", SearchContext::new("trace-marker"));
        assert!(!debug.contains("trace-marker"));
        assert!(debug.contains("12"));
    }

    #[test]
    fn gather_context_envelope_drops_absent_null_and_non_string_values() {
        assert!(SearchContext::from_request_field(None).unwrap().is_none());
        let context = SearchContext::from_request_field(Some(&json!("trace-marker")))
            .unwrap()
            .unwrap();
        assert_eq!(context.as_str(), "trace-marker");
        for value in [
            json!(null),
            json!(true),
            json!(42),
            json!(["trace-marker"]),
            json!({"trace-marker": true}),
        ] {
            assert!(SearchContext::from_request_field(Some(&value))
                .unwrap()
                .is_none());
        }
    }
}
