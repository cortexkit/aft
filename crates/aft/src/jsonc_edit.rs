//! Comment-preserving edits to a JSONC document.
//!
//! `aft setup` and `aft fix-config` rewrite a handful of keys in files people
//! edit by hand. Re-serializing the parsed value would drop every comment and
//! reorder keys, so this module instead locates the byte span of the member it
//! has to change and splices new text into the original document. Everything
//! it does not touch (comments, formatting, unrelated keys, key order) is
//! carried over byte for byte.
//!
//! The parser accepts the same dialect the config loader accepts: JSON plus
//! `//` and `/* */` comments and trailing commas.

use std::io::Write;
use std::path::Path;

use serde_json::Value;

/// A parsed value and its byte span `[start, end)` in the document.
#[derive(Debug, Clone)]
struct Node {
    start: usize,
    end: usize,
    kind: NodeKind,
}

#[derive(Debug, Clone)]
enum NodeKind {
    Object(Vec<Member>),
    Array,
    Scalar,
}

#[derive(Debug, Clone)]
struct Member {
    key: String,
    key_start: usize,
    value: Node,
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            src: text.as_bytes(),
            pos: 0,
        }
    }

    fn error(&self, message: &str) -> String {
        format!("invalid JSONC at byte {}: {message}", self.pos)
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    /// Skip whitespace and comments.
    fn skip_trivia(&mut self) -> Result<(), String> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                Some(b'/') if self.src.get(self.pos + 1) == Some(&b'/') => {
                    while let Some(byte) = self.peek() {
                        if byte == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.src.get(self.pos + 1) == Some(&b'*') => {
                    self.pos += 2;
                    loop {
                        match self.peek() {
                            None => return Err(self.error("unterminated block comment")),
                            Some(b'*') if self.src.get(self.pos + 1) == Some(&b'/') => {
                                self.pos += 2;
                                break;
                            }
                            Some(_) => self.pos += 1,
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        let start = self.pos;
        if self.peek() != Some(b'"') {
            return Err(self.error("expected string"));
        }
        self.pos += 1;
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b'\\') => self.pos += 2,
                Some(b'"') => {
                    self.pos += 1;
                    break;
                }
                Some(_) => self.pos += 1,
            }
        }
        let raw = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|_| self.error("string is not UTF-8"))?;
        serde_json::from_str::<String>(raw).map_err(|error| self.error(&error.to_string()))
    }

    fn parse_value(&mut self) -> Result<Node, String> {
        self.skip_trivia()?;
        let start = self.pos;
        match self.peek() {
            Some(b'{') => {
                self.pos += 1;
                let mut members = Vec::new();
                loop {
                    self.skip_trivia()?;
                    match self.peek() {
                        Some(b'}') => {
                            self.pos += 1;
                            break;
                        }
                        Some(b'"') => {
                            let key_start = self.pos;
                            let key = self.parse_string()?;
                            self.skip_trivia()?;
                            if self.peek() != Some(b':') {
                                return Err(self.error("expected ':'"));
                            }
                            self.pos += 1;
                            let value = self.parse_value()?;
                            members.push(Member {
                                key,
                                key_start,
                                value,
                            });
                            self.skip_trivia()?;
                            match self.peek() {
                                Some(b',') => self.pos += 1,
                                Some(b'}') => {}
                                _ => return Err(self.error("expected ',' or '}'")),
                            }
                        }
                        _ => return Err(self.error("expected object key")),
                    }
                }
                Ok(Node {
                    start,
                    end: self.pos,
                    kind: NodeKind::Object(members),
                })
            }
            Some(b'[') => {
                self.pos += 1;
                loop {
                    self.skip_trivia()?;
                    if self.peek() == Some(b']') {
                        self.pos += 1;
                        break;
                    }
                    self.parse_value()?;
                    self.skip_trivia()?;
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {}
                        _ => return Err(self.error("expected ',' or ']'")),
                    }
                }
                Ok(Node {
                    start,
                    end: self.pos,
                    kind: NodeKind::Array,
                })
            }
            Some(b'"') => {
                self.parse_string()?;
                Ok(Node {
                    start,
                    end: self.pos,
                    kind: NodeKind::Scalar,
                })
            }
            Some(_) => {
                while let Some(byte) = self.peek() {
                    if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'+' | b'.') {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                if self.pos == start {
                    return Err(self.error("unexpected character"));
                }
                let token = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or_default();
                serde_json::from_str::<Value>(token).map_err(|_| self.error("invalid literal"))?;
                Ok(Node {
                    start,
                    end: self.pos,
                    kind: NodeKind::Scalar,
                })
            }
            None => Err(self.error("unexpected end of document")),
        }
    }
}

fn parse_root(text: &str) -> Result<Node, String> {
    let mut parser = Parser::new(text);
    let root = parser.parse_value()?;
    parser.skip_trivia()?;
    if parser.pos != text.len() {
        return Err(parser.error("unexpected content after the root value"));
    }
    if !matches!(root.kind, NodeKind::Object(_)) {
        return Err("invalid JSONC: the root value must be an object".to_string());
    }
    Ok(root)
}

/// Leading whitespace of the line containing `pos`.
fn line_indent(text: &str, pos: usize) -> &str {
    let line_start = text[..pos].rfind('\n').map_or(0, |index| index + 1);
    let rest = &text[line_start..];
    let width = rest
        .bytes()
        .take_while(|byte| *byte == b' ' || *byte == b'\t')
        .count();
    &rest[..width]
}

/// Whether only spaces/tabs precede `pos` on its line.
fn starts_line(text: &str, pos: usize) -> bool {
    let line_start = text[..pos].rfind('\n').map_or(0, |index| index + 1);
    text[line_start..pos]
        .bytes()
        .all(|byte| byte == b' ' || byte == b'\t')
}

/// Serialize `value` for insertion at a position whose line is indented by
/// `indent`. Arrays of scalars stay on one line; objects are expanded.
fn render_value(value: &Value, indent: &str) -> String {
    match value {
        Value::Object(map) if !map.is_empty() => {
            let inner = format!("{indent}  ");
            let body = map
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{inner}{}: {}",
                        serde_json::to_string(key).unwrap_or_default(),
                        render_value(value, &inner)
                    )
                })
                .collect::<Vec<_>>()
                .join(",\n");
            format!("{{\n{body}\n{indent}}}")
        }
        Value::Array(items) if items.iter().any(|item| item.is_object() || item.is_array()) => {
            let inner = format!("{indent}  ");
            let body = items
                .iter()
                .map(|item| format!("{inner}{}", render_value(item, &inner)))
                .collect::<Vec<_>>()
                .join(",\n");
            format!("[\n{body}\n{indent}]")
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|item| serde_json::to_string(item).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Build `{a: {b: value}}` for the remaining path segments.
fn nest(path: &[&str], value: &Value) -> Value {
    path.iter().rev().fold(value.clone(), |inner, key| {
        let mut map = serde_json::Map::new();
        map.insert((*key).to_string(), inner);
        Value::Object(map)
    })
}

/// A JSONC document edited in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsoncDocument {
    text: String,
}

impl JsoncDocument {
    /// Parse `text`. An empty (or whitespace-only) document is an empty object.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = if text.trim().is_empty() {
            "{\n}\n".to_string()
        } else {
            text.to_string()
        };
        parse_root(&text)?;
        Ok(Self { text })
    }

    /// The current document text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The plain JSON value of the document (comments and trailing commas removed).
    pub fn value(&self) -> Result<Value, String> {
        serde_json::from_str(&crate::jsonc::strip_jsonc(&self.text))
            .map_err(|error| format!("invalid JSONC: {error}"))
    }

    /// Whether a member exists at `path`.
    pub fn contains(&self, path: &[&str]) -> bool {
        let Ok(root) = parse_root(&self.text) else {
            return false;
        };
        let mut node = &root;
        for key in path {
            let NodeKind::Object(members) = &node.kind else {
                return false;
            };
            match members.iter().rev().find(|member| member.key == *key) {
                Some(member) => node = &member.value,
                None => return false,
            }
        }
        true
    }

    /// Set the member at `path` to `value`, creating missing parent objects.
    /// An existing non-object parent is replaced by an object.
    pub fn set(&mut self, path: &[&str], value: &Value) -> Result<(), String> {
        if path.is_empty() {
            return Err("cannot replace the document root".to_string());
        }
        let root = parse_root(&self.text)?;
        let mut node = root;
        for (depth, key) in path.iter().enumerate() {
            let NodeKind::Object(members) = node.kind.clone() else {
                let replacement = nest(&path[depth..], value);
                let indent = line_indent(&self.text, node.start).to_string();
                self.splice(node.start, node.end, &render_value(&replacement, &indent));
                return Ok(());
            };
            // Duplicate keys resolve to the last occurrence, as in the loader.
            match members.iter().rev().find(|member| member.key == *key) {
                Some(member) if depth + 1 == path.len() => {
                    let indent = line_indent(&self.text, member.key_start).to_string();
                    self.splice(
                        member.value.start,
                        member.value.end,
                        &render_value(value, &indent),
                    );
                    return Ok(());
                }
                Some(member) => node = member.value.clone(),
                None => {
                    let inserted = nest(&path[depth + 1..], value);
                    self.insert_member(&node, &members, key, &inserted);
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn insert_member(&mut self, object: &Node, members: &[Member], key: &str, value: &Value) {
        let key_text = serde_json::to_string(key).unwrap_or_default();
        match members.last() {
            Some(last) => {
                let first = &members[0];
                if starts_line(&self.text, first.key_start) {
                    let indent = line_indent(&self.text, first.key_start).to_string();
                    let text = format!(",\n{indent}{key_text}: {}", render_value(value, &indent));
                    self.splice(last.value.end, last.value.end, &text);
                } else {
                    let indent = line_indent(&self.text, object.start).to_string();
                    let text = format!(", {key_text}: {}", render_value(value, &indent));
                    self.splice(last.value.end, last.value.end, &text);
                }
            }
            None => {
                let close = object.end - 1;
                let close_indent = if starts_line(&self.text, close) {
                    line_indent(&self.text, close).to_string()
                } else {
                    line_indent(&self.text, object.start).to_string()
                };
                let indent = format!("{close_indent}  ");
                let inner_has_newline = self.text[object.start + 1..close].contains('\n');
                let mut text = format!("\n{indent}{key_text}: {}", render_value(value, &indent));
                if !inner_has_newline {
                    text.push('\n');
                    text.push_str(&close_indent);
                }
                self.splice(object.start + 1, object.start + 1, &text);
            }
        }
    }

    /// Remove the member at `path`. Returns whether anything was removed.
    pub fn remove(&mut self, path: &[&str]) -> Result<bool, String> {
        let Some((last_key, parents)) = path.split_last() else {
            return Err("cannot remove the document root".to_string());
        };
        let root = parse_root(&self.text)?;
        let mut node = root;
        for key in parents {
            let NodeKind::Object(members) = &node.kind else {
                return Ok(false);
            };
            match members.iter().rev().find(|member| member.key == *key) {
                Some(member) => node = member.value.clone(),
                None => return Ok(false),
            }
        }
        let NodeKind::Object(members) = &node.kind else {
            return Ok(false);
        };
        let Some(index) = members.iter().rposition(|member| member.key == *last_key) else {
            return Ok(false);
        };
        let member = &members[index];
        let bytes = self.text.as_bytes();

        let own_line = starts_line(&self.text, member.key_start);
        let begin = if own_line {
            self.text[..member.key_start]
                .rfind('\n')
                .map_or(0, |index| index + 1)
        } else {
            member.key_start
        };
        let mut end = member.value.end;
        let mut probe = end;
        while matches!(bytes.get(probe), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            probe += 1;
        }
        let had_following_comma = bytes.get(probe) == Some(&b',');
        if had_following_comma {
            end = probe + 1;
        }
        if own_line {
            let mut tail = end;
            while matches!(bytes.get(tail), Some(b' ' | b'\t' | b'\r')) {
                tail += 1;
            }
            if bytes.get(tail) == Some(&b'\n') {
                end = tail + 1;
            }
        }

        // Without a following comma the member was last: drop the comma that
        // separated it from its predecessor instead.
        let mut preceding_comma = None;
        if !had_following_comma && index > 0 {
            let mut parser = Parser::new(&self.text);
            parser.pos = members[index - 1].value.end;
            parser.skip_trivia()?;
            if parser.peek() == Some(b',') {
                preceding_comma = Some(parser.pos);
            }
        }
        self.splice(begin, end, "");
        if let Some(comma) = preceding_comma {
            self.splice(comma, comma + 1, "");
        }
        Ok(true)
    }

    /// Remove the object at `path` when it exists and has no members left.
    pub fn remove_if_empty_object(&mut self, path: &[&str]) -> Result<bool, String> {
        match self.value()?.pointer(&pointer(path)) {
            Some(Value::Object(map)) if map.is_empty() => self.remove(path),
            _ => Ok(false),
        }
    }

    fn splice(&mut self, start: usize, end: usize, replacement: &str) {
        self.text.replace_range(start..end, replacement);
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
        assert_eq!(value["indexes"], json!({"trigram": false, "semantic": false}));
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
}
