//! Comment-preserving edits to a JSONC document.
//!
//! `aft setup` and `aft fix-config` rewrite a handful of keys in files people
//! edit by hand. Re-serializing the parsed value would drop every comment and
//! reorder keys, so edits go through the concrete syntax tree of the
//! `jsonc-parser` crate (dprint's parser): it keeps every comment, blank line,
//! trailing comma and line-ending style that an edit does not touch, and fixes
//! up commas and indentation around inserted and removed members.
//!
//! The accepted dialect is the one the config loader accepts: JSON plus `//`
//! and `/* */` comments and trailing commas. The parser's other leniencies
//! (unquoted keys, single quotes, hex numbers, missing commas) are switched
//! off, because the loader would reject a file that used them.

use std::io::Write;
use std::path::Path;

use jsonc_parser::cst::{CstInputValue, CstObject, CstObjectProp, CstRootNode};
use jsonc_parser::ParseOptions;
use serde_json::Value;

fn loader_dialect() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

fn to_input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(*value),
        Value::Number(number) => CstInputValue::Number(number.to_string()),
        Value::String(text) => CstInputValue::String(text.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(to_input).collect()),
        Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), to_input(value)))
                .collect(),
        ),
    }
}

/// The member named `key`. With duplicate keys the last one wins, as it does
/// for the loader.
fn last_prop(object: &CstObject, key: &str) -> Option<CstObjectProp> {
    object
        .properties()
        .into_iter()
        .rev()
        .find(|prop| prop.decoded_name().as_deref() == Some(key))
}

fn child_object(object: &CstObject, key: &str) -> Option<CstObject> {
    last_prop(object, key)?.object_value()
}

/// A JSONC document edited in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsoncDocument {
    text: String,
}

impl JsoncDocument {
    /// Parse `text`. The root must be an object; an empty (or
    /// whitespace-only) document stands for an empty object.
    pub fn parse(text: &str) -> Result<Self, String> {
        let doc = Self {
            text: text.to_string(),
        };
        let root = doc.root()?;
        if root.value().is_some() && root.object_value().is_none() {
            return Err("invalid JSONC: the root value must be an object".to_string());
        }
        Ok(doc)
    }

    fn root(&self) -> Result<CstRootNode, String> {
        CstRootNode::parse(&self.text, &loader_dialect())
            .map_err(|error| format!("invalid JSONC: {error}"))
    }

    /// The current document text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The plain JSON value of the document (comments and trailing commas
    /// removed). An empty document is `{}`.
    pub fn value(&self) -> Result<Value, String> {
        if self.text.trim().is_empty() {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        serde_json::from_str(&crate::jsonc::strip_jsonc(&self.text))
            .map_err(|error| format!("invalid JSONC: {error}"))
    }

    /// Whether a member exists at `path`.
    pub fn contains(&self, path: &[&str]) -> bool {
        let Some((last, parents)) = path.split_last() else {
            return true;
        };
        let Ok(root) = self.root() else {
            return false;
        };
        let Some(mut object) = root.object_value() else {
            return false;
        };
        for key in parents {
            match child_object(&object, key) {
                Some(child) => object = child,
                None => return false,
            }
        }
        last_prop(&object, last).is_some()
    }

    /// Set the member at `path` to `value`, creating missing parent objects.
    /// An existing non-object parent is replaced by an object.
    pub fn set(&mut self, path: &[&str], value: &Value) -> Result<(), String> {
        let Some((last, parents)) = path.split_last() else {
            return Err("cannot replace the document root".to_string());
        };
        let root = self.root()?;
        let mut object = root.object_value_or_set();
        for key in parents {
            object = match last_prop(&object, key) {
                Some(prop) => prop.object_value_or_set(),
                None => object.object_value_or_set(key),
            };
        }
        match last_prop(&object, last) {
            Some(prop) => prop.set_value(to_input(value)),
            None => {
                object.append(last, to_input(value));
            }
        }
        self.text = root.to_string();
        Ok(())
    }

    /// Remove the member at `path`. Returns whether anything was removed.
    pub fn remove(&mut self, path: &[&str]) -> Result<bool, String> {
        let Some((last, parents)) = path.split_last() else {
            return Err("cannot remove the document root".to_string());
        };
        let root = self.root()?;
        let Some(mut object) = root.object_value() else {
            return Ok(false);
        };
        for key in parents {
            match child_object(&object, key) {
                Some(child) => object = child,
                None => return Ok(false),
            }
        }
        let Some(prop) = last_prop(&object, last) else {
            return Ok(false);
        };
        prop.remove();
        self.text = root.to_string();
        Ok(true)
    }

    /// Remove the object at `path` when it exists and has no members left.
    pub fn remove_if_empty_object(&mut self, path: &[&str]) -> Result<bool, String> {
        match self.value()?.pointer(&pointer(path)) {
            Some(Value::Object(map)) if map.is_empty() => self.remove(path),
            _ => Ok(false),
        }
    }
}

/// JSON pointer for a key path.
pub fn pointer(path: &[&str]) -> String {
    path.iter()
        .map(|key| format!("/{}", key.replace('~', "~0").replace('/', "~1")))
        .collect()
}

/// Replace `path` with `contents` atomically: write a sibling temporary file,
/// flush it, then rename it over the target. A reader sees either the old or
/// the new file, never a partial write. Existing permissions are kept.
pub fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default();
    let temp = parent.join(format!(".{name}.tmp-{}-{nanos}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        if let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(&temp, metadata.permissions())?;
        }
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const COMMENTED: &str = r#"{
  // keep me
  "$schema": "https://example/schema.json",
  /* block */
  "edit_mode": "hashline", // trailing
  "indexes": {
    "trigram": false,
  },
}
"#;

    #[test]
    fn replacing_a_value_keeps_comments_and_other_keys() {
        let mut doc = JsoncDocument::parse(COMMENTED).unwrap();
        doc.set(&["indexes", "trigram"], &json!(true)).unwrap();
        let text = doc.text();
        assert!(text.contains("// keep me"));
        assert!(text.contains("/* block */"));
        assert!(text.contains("\"edit_mode\": \"hashline\", // trailing"));
        assert!(text.contains("\"trigram\": true,"));
        assert_eq!(doc.value().unwrap()["indexes"]["trigram"], json!(true));
    }

    #[test]
    fn inserting_members_creates_parents_and_matches_indentation() {
        let mut doc = JsoncDocument::parse(COMMENTED).unwrap();
        doc.set(&["indexes", "semantic"], &json!(false)).unwrap();
        doc.set(&["github", "write"], &json!(true)).unwrap();
        doc.set(&["disabled_tools"], &json!([])).unwrap();
        let value = doc.value().unwrap();
        assert_eq!(
            value["indexes"],
            json!({"trigram": false, "semantic": false})
        );
        assert_eq!(value["github"], json!({"write": true}));
        assert_eq!(value["disabled_tools"], json!([]));
        assert!(doc.text().contains("\n  \"disabled_tools\": []"));
        assert!(doc.text().contains("// keep me"));
    }

    #[test]
    fn empty_documents_and_objects_accept_members() {
        let mut doc = JsoncDocument::parse("").unwrap();
        doc.set(&["disabled_tools"], &json!(["aft_move"])).unwrap();
        assert_eq!(doc.text(), "{\n  \"disabled_tools\": [\"aft_move\"]\n}\n");
        let mut doc = JsoncDocument::parse("{}").unwrap();
        doc.set(&["a", "b"], &json!(1)).unwrap();
        assert_eq!(doc.value().unwrap(), json!({"a": {"b": 1}}));
    }

    #[test]
    fn removing_members_fixes_commas_and_keeps_comments() {
        let mut doc = JsoncDocument::parse(COMMENTED).unwrap();
        assert!(doc.remove(&["edit_mode"]).unwrap());
        assert!(doc.remove(&["indexes"]).unwrap());
        assert!(!doc.remove(&["missing"]).unwrap());
        let value = doc.value().unwrap();
        assert_eq!(value, json!({"$schema": "https://example/schema.json"}));
        assert!(doc.text().contains("// keep me"));

        let mut doc = JsoncDocument::parse("{\"a\": 1, \"b\": 2}").unwrap();
        doc.remove(&["b"]).unwrap();
        assert_eq!(doc.value().unwrap(), json!({"a": 1}));
        let mut doc = JsoncDocument::parse("{\n  \"a\": 1,\n  \"b\": 2\n}").unwrap();
        doc.remove(&["b"]).unwrap();
        assert_eq!(doc.text(), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn empty_containers_can_be_dropped() {
        let mut doc = JsoncDocument::parse("{\"gh_shim\": {\"enabled\": true}, \"x\": 1}").unwrap();
        doc.remove(&["gh_shim", "enabled"]).unwrap();
        assert!(doc.remove_if_empty_object(&["gh_shim"]).unwrap());
        assert_eq!(doc.value().unwrap(), json!({"x": 1}));
    }

    #[test]
    fn invalid_documents_are_refused() {
        assert!(JsoncDocument::parse("[1]").is_err());
        assert!(JsoncDocument::parse("{\"a\": }").is_err());
        assert!(JsoncDocument::parse("{\"a\": 1} trailing").is_err());
    }

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("aft.jsonc");
        write_atomic(&path, "{\"a\": 1}\n").unwrap();
        write_atomic(&path, "{\"a\": 2}\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\": 2}\n");
        let entries: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn a_comment_between_members_stays_put_through_edits() {
        let text = "{\n  \"a\": 1,\n  // about b\n  \"b\": 2,\n  /* about c */\n  \"c\": 3\n}\n";
        let mut doc = JsoncDocument::parse(text).unwrap();
        doc.set(&["b"], &json!(20)).unwrap();
        doc.remove(&["c"]).unwrap();
        doc.set(&["d"], &json!(true)).unwrap();
        let out = doc.text();
        assert!(out.contains("  // about b\n  \"b\": 20"), "{out}");
        assert_eq!(doc.value().unwrap(), json!({"a": 1, "b": 20, "d": true}));
    }

    #[test]
    fn trailing_commas_are_kept_and_edits_stay_loadable() {
        let text = "{\n  \"a\": [1, 2,],\n  \"b\": {\"x\": 1,},\n}\n";
        let mut doc = JsoncDocument::parse(text).unwrap();
        doc.set(&["c"], &json!(false)).unwrap();
        doc.set(&["b", "y"], &json!(2)).unwrap();
        doc.remove(&["a"]).unwrap();
        assert_eq!(
            doc.value().unwrap(),
            json!({"b": {"x": 1, "y": 2}, "c": false})
        );
        assert!(doc.text().trim_end().ends_with(",\n}"), "{}", doc.text());
    }

    #[test]
    fn inline_nested_objects_accept_edits() {
        let text = "{\"github\": {\"read\": true}, \"indexes\": {\"semantic\": false}}";
        let mut doc = JsoncDocument::parse(text).unwrap();
        doc.set(&["github", "write"], &json!(true)).unwrap();
        doc.set(&["indexes", "semantic"], &json!(true)).unwrap();
        doc.remove(&["github", "read"]).unwrap();
        assert_eq!(
            doc.value().unwrap(),
            json!({"github": {"write": true}, "indexes": {"semantic": true}})
        );
    }

    #[test]
    fn crlf_line_endings_are_preserved() {
        let text = "{\r\n  // note\r\n  \"a\": 1\r\n}\r\n";
        let mut doc = JsoncDocument::parse(text).unwrap();
        doc.set(&["b"], &json!(["x"])).unwrap();
        doc.set(&["c", "d"], &json!(true)).unwrap();
        let out = doc.text();
        assert!(
            !out.replace("\r\n", "").contains('\n'),
            "bare LF in {out:?}"
        );
        assert!(out.contains("// note\r\n"));
        assert_eq!(
            doc.value().unwrap(),
            json!({"a": 1, "b": ["x"], "c": {"d": true}})
        );
    }

    #[test]
    fn empty_object_and_empty_file_both_accept_members() {
        for text in ["{}", "", "  \n", "{}\n"] {
            let mut doc = JsoncDocument::parse(text).unwrap();
            assert_eq!(doc.value().unwrap(), json!({}));
            assert!(!doc.remove(&["x"]).unwrap());
            doc.set(&["disabled_tools"], &json!([])).unwrap();
            doc.set(&["github", "write"], &json!(true)).unwrap();
            assert_eq!(
                doc.value().unwrap(),
                json!({"disabled_tools": [], "github": {"write": true}}),
                "{text:?} -> {}",
                doc.text()
            );
        }
    }

    #[test]
    fn duplicate_keys_edit_the_last_occurrence_like_the_loader() {
        let mut doc = JsoncDocument::parse("{\"a\": 1, \"a\": 2}").unwrap();
        doc.set(&["a"], &json!(3)).unwrap();
        assert_eq!(doc.value().unwrap(), json!({"a": 3}));
    }
}
