//! AFT's implementation of the rich-UI schema language, as a checker.
//!
//! Design 2.2 of occipital's registry design defines a bounded schema language
//! that a phone can check without a general JSON Schema engine. AFT publishes
//! its operation schemas in it and validates every projected carrier reply
//! against them, so the checker is the one place that decides two separate
//! questions:
//!
//! - **Is a published schema well formed?** `check_schema` accepts only the
//!   constructs design 2.2 lists — `null`, `boolean`, `integer`, `decimal`,
//!   `string`, `list` and `record` — with depth at most 8 and no unions,
//!   references or patterns. A construct outside that set is a `SchemaError`,
//!   so a published schema can never smuggle in a shape the phone would not
//!   understand.
//! - **Does a reply fit a schema?** `validate` checks a JSON value against a
//!   well-formed schema, and reports the deepest failing path. Records are
//!   closed: a key the schema does not list is a failure, not something to
//!   forward.
//!
//! The canonical spelling of a construct here is the one design 2.2 and the
//! registry design's sketch use: a type name in `type`, with the per-type
//! keywords beside it (`min`/`max`/`scale` for `decimal`, `maxBytes`/`enum`
//! for `string`, `items`/`maxItems` for `list`, `fields` for `record`). A
//! schema that carries `type` from the wrong construct, a keyword its type
//! does not define, or a keyword a different construct defines is refused,
//! so one semantic schema has exactly one spelling.

use std::collections::BTreeSet;
use std::fmt;

use serde_json::Value;

use super::jcs_number::canonical_number_text;

/// The deepest nesting design 2.2 allows ("Depth ≤8"). A record that holds a
/// list of records reaches depth 3.
pub(crate) const MAX_DEPTH: usize = 8;

/// The record field flags design 2.2 names. They mark a field inside
/// `record.fields` and are not constructs of their own, so a schema that
/// carries one outside `fields` was written for a language this is not.
pub(crate) const FIELD_FLAGS: [&str; 2] = ["nullable", "optional"];

/// The constructs design 2.2's schema language has, one per published `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Construct {
    Null,
    Boolean,
    Integer,
    Decimal,
    String,
    List,
    Record,
}

impl Construct {
    /// The `type` keyword value for this construct.
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Construct::Null => "null",
            Construct::Boolean => "boolean",
            Construct::Integer => "integer",
            Construct::Decimal => "decimal",
            Construct::String => "string",
            Construct::List => "list",
            Construct::Record => "record",
        }
    }

    /// Keywords that only a different construct defines. Carrying one names a
    /// construct the schema language has no form for, which is how a union
    /// (`{"type":"integer","fields":{...}}`) enters: it is refused rather
    /// than silently read as an integer.
    fn reserved_by_another(self) -> &'static [&'static str] {
        match self {
            Construct::Null | Construct::Boolean => &[
                "min", "max", "scale", "maxBytes", "enum", "items", "maxItems", "fields",
            ],
            Construct::Integer => &["scale", "maxBytes", "enum", "items", "maxItems", "fields"],
            Construct::Decimal => &["maxBytes", "enum", "items", "maxItems", "fields"],
            Construct::String => &["min", "max", "scale", "items", "maxItems", "fields"],
            Construct::List => &["min", "max", "scale", "maxBytes", "enum", "fields"],
            Construct::Record => &[
                "min", "max", "scale", "maxBytes", "enum", "items", "maxItems",
            ],
        }
    }

    fn from_keyword(text: &str) -> Option<Construct> {
        [
            Construct::Null,
            Construct::Boolean,
            Construct::Integer,
            Construct::Decimal,
            Construct::String,
            Construct::List,
            Construct::Record,
        ]
        .into_iter()
        .find(|construct| construct.keyword() == text)
    }
}

impl fmt::Display for Construct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.keyword())
    }
}

/// Why a schema is not a well-formed design 2.2 schema, or why a value does
/// not fit one.
///
/// Both answers name the path they apply to in the form
/// `schema.fields.exitCode` (or `value.items[1].text` for a reply), so a
/// failing test or a refusal message points at the exact spot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SchemaError {
    /// The schema is not a JSON object.
    NotAnObject { path: String },
    /// `type` is missing or is not one of the seven construct names.
    UnknownType { path: String, found: String },
    /// The schema uses a keyword defined for a different construct than its
    /// `type`, such as `fields` on an `integer`: a union in disguise.
    ForeignKeyword {
        path: String,
        construct: Construct,
        keyword: String,
    },
    /// The schema carries a keyword the language does not define.
    UnknownKeyword { path: String, keyword: String },
    /// A schema object lacks `type`.
    MissingType { path: String },
    /// A keyword holds a value of the wrong JSON type.
    BadKeywordType {
        path: String,
        keyword: String,
        expected: &'static str,
    },
    /// A bound is not an integer within ±(2^53-1). Schema bounds travel in the
    /// manifest, so they follow its number rule.
    BoundOutOfRange { path: String, keyword: String },
    /// A numeric bound pair is inverted (`min` above `max`).
    InvertedBounds { path: String },
    /// `decimal.scale` is outside the 0 to 20 the language allows.
    ScaleTooLarge { path: String },
    /// `string.enum` has no members.
    EmptyEnum { path: String },
    /// `string.enum` repeats a member.
    DuplicateEnumMember { path: String, member: String },
    /// A construct nests deeper than [`MAX_DEPTH`].
    TooDeep { path: String },
    /// A value does not fit the schema at `path`.
    ValueMismatch {
        path: String,
        expected: String,
        found: String,
    },
    /// A record value carries a key its schema does not list. Records are
    /// closed; the caller drops handler keys before validating, so a key here
    /// means the projection let one through.
    UnknownField { path: String, name: String },
    /// A record value lacks a field its schema marks required.
    MissingField { path: String, name: String },
    /// A list or string is longer than its schema allows.
    LimitExceeded {
        path: String,
        keyword: &'static str,
        limit: i128,
        found: i128,
    },
    /// An integer value is outside ±(2^53-1) even though the schema set no
    /// bound. Integers travel in the manifest and in carrier answers, so they
    /// follow the manifest's number rule and must be exact as a double.
    IntegerOutOfRange { path: String, value: String },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaError::NotAnObject { path } => {
                write!(f, "{path}: a schema must be a JSON object")
            }
            SchemaError::UnknownType { path, found } => write!(
                f,
                "{path}: `type` {found} is not one of null, boolean, integer, decimal, \
                 string, list or record"
            ),
            SchemaError::ForeignKeyword {
                path,
                construct,
                keyword,
            } => write!(
                f,
                "{path}: `{keyword}` is not a keyword of `{construct}`; the construct it \
                 belongs to has no union form here"
            ),
            SchemaError::UnknownKeyword { path, keyword } => {
                write!(f, "{path}: `{keyword}` is not part of the schema language")
            }
            SchemaError::MissingType { path } => {
                write!(f, "{path}: a schema must declare `type`")
            }
            SchemaError::BadKeywordType {
                path,
                keyword,
                expected,
            } => write!(f, "{path}: `{keyword}` must be {expected}"),
            SchemaError::BoundOutOfRange { path, keyword } => write!(
                f,
                "{path}: `{keyword}` must be an integer within ±9007199254740991"
            ),
            SchemaError::InvertedBounds { path } => {
                write!(f, "{path}: `min` is above `max`")
            }
            SchemaError::ScaleTooLarge { path } => {
                write!(f, "{path}: `scale` must be an integer from 0 to 20")
            }
            SchemaError::EmptyEnum { path } => write!(f, "{path}: `enum` must list a value"),
            SchemaError::DuplicateEnumMember { path, member } => {
                write!(f, "{path}: `enum` lists {member:?} twice")
            }
            SchemaError::TooDeep { path } => write!(
                f,
                "{path}: nests deeper than the language's maximum depth of {MAX_DEPTH}"
            ),
            SchemaError::ValueMismatch {
                path,
                expected,
                found,
            } => write!(f, "{path}: expected {expected}, found {found}"),
            SchemaError::UnknownField { path, name } => {
                write!(f, "{path}: the record has no field `{name}`")
            }
            SchemaError::MissingField { path, name } => {
                write!(f, "{path}: the required field `{name}` is missing")
            }
            SchemaError::LimitExceeded {
                path,
                keyword,
                limit,
                found,
            } => write!(f, "{path}: `{keyword}` is {limit}, found {found}"),
            SchemaError::IntegerOutOfRange { path, value } => write!(
                f,
                "{path}: integer {value} is outside the schema's integer range"
            ),
        }
    }
}

impl std::error::Error for SchemaError {}

/// Check that `schema` is a well-formed design 2.2 schema and return its
/// construct, which the caller uses to pick the right value check.
pub(crate) fn check_schema(schema: &Value) -> Result<Construct, SchemaError> {
    let path = String::from("schema");
    check_schema_at(schema, &path, 1)
}

fn check_schema_at(schema: &Value, path: &str, depth: usize) -> Result<Construct, SchemaError> {
    if depth > MAX_DEPTH {
        return Err(SchemaError::TooDeep {
            path: path.to_string(),
        });
    }
    let object = schema.as_object().ok_or_else(|| SchemaError::NotAnObject {
        path: path.to_string(),
    })?;
    let type_name = match object.get("type") {
        Some(Value::String(name)) => name,
        Some(other) => {
            return Err(SchemaError::UnknownType {
                path: format!("{path}.type"),
                found: value_kind(other).to_string(),
            })
        }
        None => {
            return Err(SchemaError::MissingType {
                path: path.to_string(),
            })
        }
    };
    let construct = Construct::from_keyword(type_name).ok_or_else(|| SchemaError::UnknownType {
        path: format!("{path}.type"),
        found: format!("{type_name:?}"),
    })?;

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    seen.insert("type");
    // `nullable` and `optional` are record field flags, not schema keywords.
    // Only a field inside `record.fields` may carry them, and `check_field`
    // validates those. A flag anywhere else is refused like any other
    // keyword this language does not define.
    match construct {
        Construct::Null | Construct::Boolean => {}
        Construct::Integer => {
            seen.extend(["min", "max"]);
            check_bound(object, path, "min")?;
            check_bound(object, path, "max")?;
            check_bound_order(object, path)?;
        }
        Construct::Decimal => {
            seen.extend(["min", "max", "scale"]);
            check_bound(object, path, "min")?;
            check_bound(object, path, "max")?;
            check_bound_order(object, path)?;
            if let Some(scale) = object.get("scale") {
                match scale.as_i64() {
                    Some(value) if (0..=20).contains(&value) => {}
                    Some(_) => {
                        return Err(SchemaError::ScaleTooLarge {
                            path: path.to_string(),
                        })
                    }
                    None => {
                        return Err(SchemaError::BadKeywordType {
                            path: path.to_string(),
                            keyword: "scale".to_string(),
                            expected: "an integer",
                        })
                    }
                }
            }
        }
        Construct::String => {
            seen.extend(["maxBytes", "enum"]);
            check_bound(object, path, "maxBytes")?;
            check_enum(object, path)?;
        }
        Construct::List => {
            seen.extend(["items", "maxItems"]);
            check_bound(object, path, "maxItems")?;
            let items = object
                .get("items")
                .ok_or_else(|| SchemaError::BadKeywordType {
                    path: path.to_string(),
                    keyword: "items".to_string(),
                    expected: "a schema",
                })?;
            check_schema_at(items, &format!("{path}.items"), depth + 1)?;
        }
        Construct::Record => {
            seen.insert("fields");
            let fields = object
                .get("fields")
                .and_then(Value::as_object)
                .ok_or_else(|| SchemaError::BadKeywordType {
                    path: path.to_string(),
                    keyword: "fields".to_string(),
                    expected: "an object of schemas",
                })?;
            for (name, field) in fields {
                check_field(field, &format!("{path}.fields.{name}"), depth + 1)?;
            }
        }
    }

    for keyword in object.keys() {
        if seen.contains(keyword.as_str()) {
            continue;
        }
        if construct.reserved_by_another().contains(&keyword.as_str()) {
            return Err(SchemaError::ForeignKeyword {
                path: path.to_string(),
                construct,
                keyword: keyword.clone(),
            });
        }
        return Err(SchemaError::UnknownKeyword {
            path: path.to_string(),
            keyword: keyword.clone(),
        });
    }
    Ok(construct)
}

/// A record field is a schema object that may also be marked `nullable` or
/// `optional`; both are false when absent, and both must be booleans.
fn check_field(field: &Value, path: &str, depth: usize) -> Result<Construct, SchemaError> {
    let object = field.as_object().ok_or_else(|| SchemaError::NotAnObject {
        path: path.to_string(),
    })?;
    for flag in FIELD_FLAGS {
        if let Some(flag_value) = object.get(flag) {
            if !flag_value.is_boolean() {
                return Err(SchemaError::BadKeywordType {
                    path: path.to_string(),
                    keyword: flag.to_string(),
                    expected: "a boolean",
                });
            }
        }
    }
    // The flags are valid only on a record field, which is where this
    // function is called, so they are removed before the construct check; the
    // loop above already checked that each is a boolean.
    let mut schema_only = field.clone();
    if let Some(schema_object) = schema_only.as_object_mut() {
        schema_object.remove("nullable");
        schema_object.remove("optional");
    }
    check_schema_at(&schema_only, path, depth)
}

/// A string's `enum`: a non-empty array of distinct strings.
fn check_enum(object: &serde_json::Map<String, Value>, path: &str) -> Result<(), SchemaError> {
    let Some(members) = object.get("enum") else {
        return Ok(());
    };
    let members = members
        .as_array()
        .ok_or_else(|| SchemaError::BadKeywordType {
            path: path.to_string(),
            keyword: "enum".to_string(),
            expected: "an array of strings",
        })?;
    if members.is_empty() {
        return Err(SchemaError::EmptyEnum {
            path: path.to_string(),
        });
    }
    let mut listed: BTreeSet<&str> = BTreeSet::new();
    for member in members {
        let text = member.as_str().ok_or_else(|| SchemaError::BadKeywordType {
            path: path.to_string(),
            keyword: "enum".to_string(),
            expected: "an array of strings",
        })?;
        if !listed.insert(text) {
            return Err(SchemaError::DuplicateEnumMember {
                path: path.to_string(),
                member: text.to_string(),
            });
        }
    }
    Ok(())
}

fn check_bound(
    object: &serde_json::Map<String, Value>,
    path: &str,
    keyword: &str,
) -> Result<(), SchemaError> {
    let Some(value) = object.get(keyword) else {
        return Ok(());
    };
    let Some(number) = value.as_i64().or_else(|| {
        // Accept an unsigned number only when it still fits an i64.
        value
            .as_u64()
            .filter(|n| i64::try_from(*n).is_ok())
            .map(|n| n as i64)
    }) else {
        return Err(SchemaError::BadKeywordType {
            path: path.to_string(),
            keyword: keyword.to_string(),
            expected: "an integer",
        });
    };
    if i128::from(number).abs() > super::canonical::MAX_SAFE_INTEGER {
        return Err(SchemaError::BoundOutOfRange {
            path: path.to_string(),
            keyword: keyword.to_string(),
        });
    }
    Ok(())
}

fn check_bound_order(
    object: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), SchemaError> {
    let (Some(min), Some(max)) = (object.get("min"), object.get("max")) else {
        return Ok(());
    };
    let min = min.as_i64().or_else(|| {
        min.as_u64()
            .filter(|n| i64::try_from(*n).is_ok())
            .map(|n| n as i64)
    });
    let max = max.as_i64().or_else(|| {
        max.as_u64()
            .filter(|n| i64::try_from(*n).is_ok())
            .map(|n| n as i64)
    });
    match (min, max) {
        (Some(min), Some(max)) if min > max => Err(SchemaError::InvertedBounds {
            path: path.to_string(),
        }),
        _ => Ok(()),
    }
}

/// Validate `value` against an already well-formed `schema`.
///
/// The caller runs [`check_schema`] once on each published schema; this
/// function trusts the schema's shape and only walks the value, so a
/// reply-validating hot path pays one pass, not two.
pub(crate) fn validate(schema: &Value, value: &Value) -> Result<(), SchemaError> {
    validate_at(schema, value, &String::from("value"))
}

fn validate_at(schema: &Value, value: &Value, path: &String) -> Result<(), SchemaError> {
    // Schemas reaching this function were checked, so `type` is present and
    // names a construct.
    let object = schema
        .as_object()
        .expect("a checked schema is a JSON object");
    let type_name = object
        .get("type")
        .and_then(Value::as_str)
        .expect("a checked schema declares `type`");
    let construct = Construct::from_keyword(type_name).expect("a checked schema has a construct");
    match construct {
        Construct::Null => {
            if value.is_null() {
                Ok(())
            } else {
                Err(mismatch(path, "null", value))
            }
        }
        Construct::Boolean => {
            if value.is_boolean() {
                Ok(())
            } else {
                Err(mismatch(path, "boolean", value))
            }
        }
        Construct::Integer => {
            let Some(number) = value.as_i64().or_else(|| {
                value
                    .as_u64()
                    .filter(|n| i64::try_from(*n).is_ok())
                    .map(|n| n as i64)
            }) else {
                return Err(mismatch(path, "integer", value));
            };
            let integer = i128::from(number);
            if integer.abs() > super::canonical::MAX_SAFE_INTEGER {
                return Err(SchemaError::IntegerOutOfRange {
                    path: path.clone(),
                    value: integer.to_string(),
                });
            }
            let max = object.get("max").and_then(Value::as_i64);
            let min = object.get("min").and_then(Value::as_i64);
            if max.is_some_and(|max| number > max) || min.is_some_and(|min| number < min) {
                return Err(out_of_range(path, number, min, max));
            }
            Ok(())
        }
        Construct::Decimal => {
            let Some(float) = value.as_f64() else {
                return Err(mismatch(path, "decimal", value));
            };
            let text =
                canonical_number_text(float).ok_or_else(|| mismatch(path, "decimal", value))?;
            if let Some(min) = object.get("min").and_then(Value::as_f64) {
                if float < min {
                    return Err(SchemaError::ValueMismatch {
                        path: path.clone(),
                        expected: format!("decimal at least {min}"),
                        found: text,
                    });
                }
            }
            if let Some(max) = object.get("max").and_then(Value::as_f64) {
                if float > max {
                    return Err(SchemaError::ValueMismatch {
                        path: path.clone(),
                        expected: format!("decimal at most {max}"),
                        found: text,
                    });
                }
            }
            if let Some(scale) = object.get("scale").and_then(Value::as_u64) {
                // The places are counted from the value's decimal expansion,
                // not from one spelling of it: the canonical text of a small
                // value is in exponent form (`1e-7`) and carries no decimal
                // point, yet that value has seven fractional places.
                if fractional_places(&text) > scale {
                    return Err(SchemaError::ValueMismatch {
                        path: path.clone(),
                        expected: format!("decimal with at most {scale} fractional digits"),
                        found: text,
                    });
                }
            }
            Ok(())
        }
        Construct::String => {
            let Some(text) = value.as_str() else {
                return Err(mismatch(path, "string", value));
            };
            if let Some(max_bytes) = object.get("maxBytes").and_then(Value::as_i64) {
                let found = text.len() as i128;
                if found > i128::from(max_bytes) {
                    return Err(SchemaError::LimitExceeded {
                        path: path.clone(),
                        keyword: "maxBytes",
                        limit: i128::from(max_bytes),
                        found,
                    });
                }
            }
            if let Some(members) = object.get("enum").and_then(Value::as_array) {
                let allowed = members.iter().any(|member| member.as_str() == Some(text));
                if !allowed {
                    return Err(mismatch(
                        path,
                        &format!(
                            "one of {}",
                            members
                                .iter()
                                .filter_map(Value::as_str)
                                .map(|member| format!("{member:?}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        value,
                    ));
                }
            }
            Ok(())
        }
        Construct::List => {
            let Some(items) = value.as_array() else {
                return Err(mismatch(path, "list", value));
            };
            if let Some(max_items) = object.get("maxItems").and_then(Value::as_i64) {
                let found = items.len() as i128;
                if found > i128::from(max_items) {
                    return Err(SchemaError::LimitExceeded {
                        path: path.clone(),
                        keyword: "maxItems",
                        limit: i128::from(max_items),
                        found,
                    });
                }
            }
            let item_schema = object
                .get("items")
                .expect("a checked list declares `items`");
            for (index, item) in items.iter().enumerate() {
                validate_at(item_schema, item, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        Construct::Record => {
            let Some(fields_value) = value.as_object() else {
                return Err(mismatch(path, "record", value));
            };
            let fields = object
                .get("fields")
                .and_then(Value::as_object)
                .expect("a checked record declares `fields`");
            for (name, _) in fields_value {
                if !fields.contains_key(name) {
                    return Err(SchemaError::UnknownField {
                        path: path.clone(),
                        name: name.clone(),
                    });
                }
            }
            for (name, field_schema) in fields {
                let nullable = field_schema
                    .get("nullable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let optional = field_schema
                    .get("optional")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                match fields_value.get(name) {
                    None => {
                        if !optional {
                            return Err(SchemaError::MissingField {
                                path: path.clone(),
                                name: name.clone(),
                            });
                        }
                    }
                    Some(Value::Null) if nullable => {}
                    Some(field_value) => {
                        validate_at(field_schema, field_value, &format!("{path}.{name}"))?;
                    }
                }
            }
            Ok(())
        }
    }
}

/// The number of decimal places a canonical number text shows once written
/// out in full, ignoring trailing zeros.
///
/// The text is the ECMAScript form `canonical_number_text` produces, which
/// switches to exponents at `1e-7` and `1e21`. Counting the digits after a
/// decimal point would therefore report no fraction for `1e-7` and a fraction
/// for `0.000001`; both have six or seven places, and `1e21` has none.
fn fractional_places(text: &str) -> u64 {
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (
            mantissa,
            exponent
                .parse::<i64>()
                .expect("a canonical number's exponent is an integer"),
        ),
        None => (text, 0),
    };
    let fraction = mantissa
        .split_once('.')
        .map(|(_, fraction)| fraction)
        .unwrap_or("");
    // Every digit after the point in the mantissa is a place, leading zeros
    // included: `0.000001` has six, and only its trailing zeros can be
    // dropped. A negative exponent moves the point further left and adds that
    // many places; a positive one moves it right and cancels them.
    let places = fraction.trim_end_matches('0').len() as i64 - exponent;
    places.max(0) as u64
}

fn mismatch(path: &str, expected: &str, found: &Value) -> SchemaError {
    SchemaError::ValueMismatch {
        path: path.to_string(),
        expected: expected.to_string(),
        found: value_kind(found).to_string(),
    }
}

fn out_of_range(path: &str, value: i64, min: Option<i64>, max: Option<i64>) -> SchemaError {
    let expected = match (min, max) {
        (None, Some(max)) => format!("integer at most {max}"),
        (Some(min), None) => format!("integer at least {min}"),
        (Some(min), Some(max)) => format!("integer from {min} to {max}"),
        (None, None) => "integer".to_string(),
    };
    SchemaError::ValueMismatch {
        path: path.to_string(),
        expected,
        found: value.to_string(),
    }
}

/// The JSON type name of a value, for a failure message.
fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                "integer"
            } else {
                "decimal"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "record",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One schema for each construct design 2.2 defines, written the way the
    /// registry design's sketch writes it.
    fn each_construct() -> Vec<(&'static str, Value)> {
        vec![
            ("null", json!({"type": "null"})),
            ("boolean", json!({"type": "boolean"})),
            (
                "integer",
                json!({"type": "integer", "min": -2147483648, "max": 2147483647}),
            ),
            (
                "decimal",
                json!({"type": "decimal", "min": 0, "max": 100, "scale": 2}),
            ),
            ("string", json!({"type": "string", "maxBytes": 8192})),
            ("string enum", json!({"type": "string", "enum": ["a", "b"]})),
            (
                "list",
                json!({"type": "list", "items": {"type": "string"}, "maxItems": 4}),
            ),
            (
                "record",
                json!({"type": "record", "fields": {
                    "id": {"type": "string"},
                    "count": {"type": "integer", "nullable": true, "optional": true}}}),
            ),
        ]
    }

    #[test]
    fn every_construct_design_2_2_defines_is_accepted() {
        for (name, schema) in each_construct() {
            assert!(
                check_schema(&schema).is_ok(),
                "{name} construct must be accepted by the checker"
            );
        }
    }

    /// The other side of the same guard: a schema built from anything outside
    /// design 2.2 must fail, so "accepts only design 2.2 constructs" is a
    /// property the test can lose. Each entry is a construct the language
    /// deliberately does not have.
    #[test]
    fn constructs_outside_design_2_2_are_refused() {
        let refused = [
            ("an unknown type name", json!({"type": "number"})),
            ("a union of two types", json!({"type": ["integer", "null"]})),
            ("a $ref reference", json!({"$ref": "#/definitions/thing"})),
            (
                "a top-level pattern",
                json!({"type": "string", "pattern": "^a+$"}),
            ),
            (
                "a regex constraint on the list",
                json!({"type": "list", "items": {"type": "string"}, "pattern": "x"}),
            ),
            (
                "a JSON Schema anyOf",
                json!({"anyOf": [{"type": "string"}]}),
            ),
            (
                "an array-form items",
                json!({"type": "list", "items": [{"type": "string"}]}),
            ),
            ("a bare schema without type", json!({"min": 0})),
            (
                "an integer carrying record fields (a union in disguise)",
                json!({"type": "integer", "fields": {"a": {"type": "string"}}}),
            ),
            (
                "a record carrying min/max",
                json!({"type": "record", "fields": {}, "min": 0}),
            ),
            (
                "a string carrying an integer bound",
                json!({"type": "string", "min": 0}),
            ),
            (
                "a nullable flag on the construct itself",
                json!({"type": "string", "nullable": true}),
            ),
            (
                "an unknown keyword",
                json!({"type": "string", "description": "a name"}),
            ),
            ("an empty enum", json!({"type": "string", "enum": []})),
            (
                "a duplicated enum member",
                json!({"type": "string", "enum": ["a", "a"]}),
            ),
            (
                "an inverted bound pair",
                json!({"type": "integer", "min": 5, "max": 4}),
            ),
            (
                "a decimal scale above 20",
                json!({"type": "decimal", "scale": 21}),
            ),
            (
                "a bound beyond a double's exact range",
                json!({"type": "integer", "max": 9007199254740992i64}),
            ),
            ("a fractional bound", json!({"type": "decimal", "max": 1.5})),
            (
                "a record field flag on a nested construct",
                json!({"type": "list", "items": {"type": "string", "optional": true}}),
            ),
        ];
        for (description, schema) in refused {
            assert!(
                check_schema(&schema).is_err(),
                "{description} must not pass the checker: {schema}"
            );
        }
    }

    #[test]
    fn depth_is_capped_at_eight_and_the_exact_limit_passes() {
        // A list of lists ... of null: depth `levels` from the root.
        fn nested(levels: usize) -> Value {
            let mut schema = json!({"type": "null"});
            for _ in 1..levels {
                schema = json!({"type": "list", "items": schema});
            }
            schema
        }
        assert_eq!(check_schema(&nested(8)).unwrap(), Construct::List);
        assert_eq!(
            check_schema(&nested(9)),
            Err(SchemaError::TooDeep {
                path: "schema.items.items.items.items.items.items.items.items".to_string()
            })
        );
        // A record ladder reaches the same limit through `fields`.
        fn nested_records(levels: usize) -> Value {
            let mut schema = json!({"type": "null"});
            for _ in 1..levels {
                schema = json!({"type": "record", "fields": {"next": schema}});
            }
            schema
        }
        assert!(check_schema(&nested_records(8)).is_ok());
        assert!(matches!(
            check_schema(&nested_records(9)),
            Err(SchemaError::TooDeep { .. })
        ));
    }

    #[test]
    fn a_reply_must_fit_its_record_exactly() {
        let schema = json!({"type": "record", "fields": {
            "running": {"type": "boolean", "nullable": true},
            "exitCode": {"type": "integer", "nullable": true, "min": -2147483648, "max": 2147483647},
            "stdout": {"type": "string", "maxBytes": 4},
            "tag": {"type": "string", "enum": ["a", "b"], "optional": true}
        }});
        assert_eq!(check_schema(&schema).unwrap(), Construct::Record);
        assert!(validate(
            &schema,
            &json!({"running": null, "exitCode": 0, "stdout": "ab\nc"})
        )
        .is_ok());
        // A key the record does not publish is refused, never forwarded.
        assert_eq!(
            validate(
                &schema,
                &json!({"running": true, "exitCode": 1, "stdout": "", "extra": 1})
            ),
            Err(SchemaError::UnknownField {
                path: "value".to_string(),
                name: "extra".to_string()
            })
        );
        // A required field the handler did not supply is missing, never defaulted.
        assert_eq!(
            validate(&schema, &json!({"running": true, "stdout": ""})),
            Err(SchemaError::MissingField {
                path: "value".to_string(),
                name: "exitCode".to_string()
            })
        );
        // An optional field may be absent but not null (it is not nullable).
        assert!(validate(
            &schema,
            &json!({"running": null, "exitCode": 0, "stdout": ""})
        )
        .is_ok());
        assert_eq!(
            validate(
                &schema,
                &json!({"running": null, "exitCode": 0, "stdout": "", "tag": null})
            ),
            Err(SchemaError::ValueMismatch {
                path: "value.tag".to_string(),
                expected: "string".to_string(),
                found: "null".to_string()
            })
        );
        // Byte limits count UTF-8 bytes, not characters.
        assert_eq!(
            validate(
                &schema,
                &json!({"running": null, "exitCode": 0, "stdout": "ééé"})
            ),
            Err(SchemaError::LimitExceeded {
                path: "value.stdout".to_string(),
                keyword: "maxBytes",
                limit: 4,
                found: 6
            })
        );
        // The integer bound is applied.
        assert_eq!(
            validate(
                &schema,
                &json!({"running": null, "exitCode": 2147483648i64, "stdout": ""})
            ),
            Err(SchemaError::ValueMismatch {
                path: "value.exitCode".to_string(),
                expected: "integer from -2147483648 to 2147483647".to_string(),
                found: "2147483648".to_string()
            })
        );
        // An enum member outside the list is refused.
        assert!(matches!(
            validate(
                &schema,
                &json!({"running": null, "exitCode": 0, "stdout": "", "tag": "c"})
            ),
            Err(SchemaError::ValueMismatch { .. })
        ));
    }

    #[test]
    fn type_mismatches_name_the_path_and_the_expected_construct() {
        let schema = json!({"type": "record", "fields": {
            "items": {"type": "list", "items": {"type": "record", "fields": {
                "text": {"type": "string"}}}}
        }});
        assert_eq!(
            validate(&schema, &json!({"items": [{"text": "ok"}, {"text": 7}]})),
            Err(SchemaError::ValueMismatch {
                path: "value.items[1].text".to_string(),
                expected: "string".to_string(),
                found: "integer".to_string()
            })
        );
        assert_eq!(
            validate(&schema, &json!({"items": "not a list"})),
            Err(SchemaError::ValueMismatch {
                path: "value.items".to_string(),
                expected: "list".to_string(),
                found: "string".to_string()
            })
        );
    }

    #[test]
    fn integers_beyond_a_doubles_exact_range_are_refused_even_unbounded() {
        let schema = json!({"type": "integer"});
        assert!(validate(&schema, &json!({"n": 0}).get("n").cloned().unwrap()).is_ok());
        assert_eq!(
            validate(&schema, &json!(9007199254740992i64)),
            Err(SchemaError::IntegerOutOfRange {
                path: "value".to_string(),
                value: "9007199254740992".to_string()
            })
        );
        assert!(validate(&schema, &json!(-9007199254740991i64)).is_ok());
        // A value the language calls a decimal is not an integer.
        assert!(matches!(
            validate(&schema, &json!(1.5)),
            Err(SchemaError::ValueMismatch { .. })
        ));
    }

    #[test]
    fn decimals_respect_their_bounds_and_stated_scale() {
        let schema = json!({"type": "decimal", "min": 0, "max": 100, "scale": 2});
        assert!(validate(&schema, &json!(12.25)).is_ok());
        assert!(validate(&schema, &json!(100)).is_ok());
        assert!(matches!(
            validate(&schema, &json!(100.5)),
            Err(SchemaError::ValueMismatch { .. })
        ));
        assert!(matches!(
            validate(&schema, &json!(0.125)),
            Err(SchemaError::ValueMismatch { .. })
        ));
        assert!(matches!(
            validate(&schema, &json!(-1)),
            Err(SchemaError::ValueMismatch { .. })
        ));
    }

    /// `scale` counts decimal places in the value itself, not in one spelling
    /// of it. A small value is written in exponent form (`1e-7`), which has no
    /// decimal point at all, and it has seven fractional places anyway; a
    /// whole value written with an exponent (`1e21`) has none.
    #[test]
    fn decimal_scale_counts_fractional_places_not_spelling_punctuation() {
        let two_places = json!({"type": "decimal", "scale": 2});

        // Around ECMAScript's switch to exponent form: 0.000001 and 1e-7 are
        // both past two places, one spelled with a decimal point and one with
        // an exponent, and both must be refused. A check that read the
        // exponent form as having no fraction would accept 1e-7 here.
        assert!(matches!(
            validate(&two_places, &json!(0.000001)),
            Err(SchemaError::ValueMismatch { .. })
        ));
        assert!(matches!(
            validate(&two_places, &json!(1e-7)),
            Err(SchemaError::ValueMismatch { .. })
        ));

        // Controls that pin each value's own place count: six places admits
        // 0.000001 and refuses 1e-7; seven admits both. A check that treated
        // the exponent form as zero places could not refuse 1e-7 at six.
        let six_places = json!({"type": "decimal", "scale": 6});
        assert!(validate(&six_places, &json!(0.000001)).is_ok());
        assert!(matches!(
            validate(&six_places, &json!(1e-7)),
            Err(SchemaError::ValueMismatch { .. })
        ));
        let seven_places = json!({"type": "decimal", "scale": 7});
        assert!(validate(&seven_places, &json!(1e-7)).is_ok());

        // Values that lose their exponent when written out: 1.5e-6 is
        // 0.0000015 (seven places) and 1.5e-2 is 0.015 (three).
        assert_eq!(canonical_number_text(1.5e-6).as_deref(), Some("0.0000015"));
        assert_eq!(canonical_number_text(1.5e-2).as_deref(), Some("0.015"));
        assert!(matches!(
            validate(&two_places, &json!(1.5e-6)),
            Err(SchemaError::ValueMismatch { .. })
        ));
        assert!(matches!(
            validate(&two_places, &json!(1.5e-2)),
            Err(SchemaError::ValueMismatch { .. })
        ));
        let three_places = json!({"type": "decimal", "scale": 3});
        assert!(validate(&three_places, &json!(1.5e-2)).is_ok());

        // A whole value has no fractional places however it is spelled,
        // including the exponent form 1e21 that ECMAScript uses for it.
        assert_eq!(canonical_number_text(1e21).as_deref(), Some("1e+21"));
        assert!(validate(&two_places, &json!(1e21)).is_ok());
        assert!(validate(&two_places, &json!(100)).is_ok());

        // The exact limit still passes, and one place past it is refused.
        assert!(validate(&two_places, &json!(0.01)).is_ok());
        assert!(matches!(
            validate(&two_places, &json!(0.001)),
            Err(SchemaError::ValueMismatch { .. })
        ));
    }
}
