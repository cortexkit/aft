//! Spelling of rendered ES import statements: the quote around the module
//! specifier and whether the statement ends with a semicolon.
//!
//! Formatters such as Biome and Prettier reject a file whose imports do not
//! match the project's style, so every place that renders an ES import asks
//! this module how to spell it. A rewritten statement keeps its own spelling;
//! a new statement follows the file's other imports, then the nearest
//! formatter config, then a fixed default.

use std::io::Read;
use std::path::Path;

use crate::parser::{parse_source_with_cached_parser, LangId};

/// How one ES import statement is spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EsImportStyle {
    pub quote: char,
    pub semicolon: bool,
}

impl Default for EsImportStyle {
    /// The generator's long-standing spelling, used when there is no file to
    /// match (for example a caller that only renders a line for display).
    fn default() -> Self {
        Self {
            quote: '\'',
            semicolon: true,
        }
    }
}

/// One module-source statement's spelling, as seen in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ObservedStyle {
    quote: Option<char>,
    semicolon: bool,
}

/// Count only module-source strings, not quotes in comments, bindings or attributes.
pub fn statement_quote(source: &str) -> Option<char> {
    let tree =
        parse_source_with_cached_parser(Path::new("quotes.ts"), source, LangId::TypeScript).ok()?;
    module_styles(tree.root_node(), source)
        .into_iter()
        .next()
        .and_then(|style| style.quote)
}

/// Spelling of an existing statement given its source text, so a rewrite of
/// that statement (removing or adding a name, organizing) changes nothing but
/// its bindings.
pub fn statement_style(raw: &str) -> EsImportStyle {
    EsImportStyle {
        quote: statement_quote(raw).unwrap_or(EsImportStyle::default().quote),
        semicolon: ends_with_semicolon(raw),
    }
}

/// The statement node's text ends at its terminator, so a trailing `;` is the
/// explicit semicolon; a statement ended by a newline has none.
fn ends_with_semicolon(statement: &str) -> bool {
    statement.trim_end().ends_with(';')
}

fn module_styles(node: tree_sitter::Node<'_>, source: &str) -> Vec<ObservedStyle> {
    let mut styles = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "import_statement" | "export_statement") {
            if let Some(module) = child.child_by_field_name("source") {
                let quote = match source[module.byte_range()].chars().next() {
                    Some(quote @ ('\'' | '"')) => Some(quote),
                    _ => None,
                };
                styles.push(ObservedStyle {
                    quote,
                    semicolon: ends_with_semicolon(&source[child.byte_range()]),
                });
            }
        }
    }
    styles
}

/// Spelling for a new import statement in `file`: the majority of the file's
/// existing module statements, ties going to the first statement, then the
/// nearest Biome or Prettier setting under `root`, then double quotes and
/// semicolons.
pub fn preferred_style(
    source: &str,
    tree: &tree_sitter::Tree,
    lang: LangId,
    file: &Path,
    root: Option<&Path>,
) -> EsImportStyle {
    let styles = if lang == LangId::Vue {
        super::vue_script_content_range(tree)
            .and_then(|(start, end)| {
                let script = &source[start..end];
                parse_source_with_cached_parser(file, script, LangId::TypeScript)
                    .ok()
                    .map(|tree| module_styles(tree.root_node(), script))
            })
            .unwrap_or_default()
    } else {
        module_styles(tree.root_node(), source)
    };

    let quotes: Vec<char> = styles.iter().filter_map(|style| style.quote).collect();
    let quote =
        majority(&quotes, '\'', '"').unwrap_or_else(|| config_quote(file, root).unwrap_or('"'));
    let semicolons: Vec<bool> = styles.iter().map(|style| style.semicolon).collect();
    let semicolon = majority(&semicolons, false, true)
        .unwrap_or_else(|| config_semicolon(file, root).unwrap_or(true));
    EsImportStyle { quote, semicolon }
}

/// The more common of two values, the first value on a tie, or `None` when
/// there is nothing to count.
fn majority<T: Copy + PartialEq>(values: &[T], a: T, b: T) -> Option<T> {
    let a_count = values.iter().filter(|&&value| value == a).count();
    let b_count = values.iter().filter(|&&value| value == b).count();
    match a_count.cmp(&b_count) {
        std::cmp::Ordering::Greater => Some(a),
        std::cmp::Ordering::Less => Some(b),
        std::cmp::Ordering::Equal => values.first().copied(),
    }
}

test_support_visibility! {
fn config_quote(file: &Path, root: Option<&Path>) -> Option<char> {
    config_setting(file, root, |name, json, text| {
        if name.starts_with("biome") {
            return match json?
                .pointer("/javascript/formatter/quoteStyle")?
                .as_str()?
            {
                "single" => Some('\''),
                "double" => Some('"'),
                _ => None,
            };
        }
        prettier_bool(name, json, text, "singleQuote").map(|single| if single { '\'' } else { '"' })
    })
}
}

test_support_visibility! {
fn config_semicolon(file: &Path, root: Option<&Path>) -> Option<bool> {
    config_setting(file, root, |name, json, text| {
        if name.starts_with("biome") {
            return match json?
                .pointer("/javascript/formatter/semicolons")?
                .as_str()?
            {
                "always" => Some(true),
                "asNeeded" => Some(false),
                _ => None,
            };
        }
        prettier_bool(name, json, text, "semi")
    })
}
}

/// A boolean Prettier option from a config file, or from the `prettier` key
/// of `package.json`.
fn prettier_bool(
    name: &str,
    json: Option<&serde_json::Value>,
    text: &str,
    key: &str,
) -> Option<bool> {
    let value = if name == "package.json" {
        json.and_then(|v| v.get("prettier"))
    } else {
        json
    };
    if let Some(setting) = value.and_then(|v| v.get(key)).and_then(|v| v.as_bool()) {
        return Some(setting);
    }
    // Static YAML/JS/TOML settings are readable without executing project code.
    if name != "package.json" && json.is_none() {
        let setting =
            regex::Regex::new(&format!(r#"\b{key}[\"']?\s*[:=]\s*(true|false)\b"#)).ok()?;
        return setting
            .captures(text)
            .map(|captures| &captures[1] == "true");
    }
    None
}

/// The first setting `read` finds in the formatter configs nearest to `file`,
/// searching from the file's directory up to `root` and never above it.
fn config_setting<T>(
    file: &Path,
    root: Option<&Path>,
    read: impl Fn(&str, Option<&serde_json::Value>, &str) -> Option<T>,
) -> Option<T> {
    let root = root?;
    let mut dir = file.parent()?;
    if !dir.starts_with(root) {
        return None;
    }
    let mut reads = 0;
    loop {
        for name in [
            "biome.json",
            "biome.jsonc",
            ".prettierrc",
            ".prettierrc.json",
            ".prettierrc.json5",
            ".prettierrc.yaml",
            ".prettierrc.yml",
            ".prettierrc.js",
            ".prettierrc.cjs",
            ".prettierrc.mjs",
            ".prettierrc.toml",
            "prettier.config.js",
            "prettier.config.cjs",
            "prettier.config.mjs",
            "package.json",
        ] {
            let Ok(mut config) = std::fs::File::open(dir.join(name)) else {
                continue;
            };
            reads += 1;
            if reads > 4 {
                return None;
            }
            let mut text = String::new();
            if config
                .by_ref()
                .take(65_537)
                .read_to_string(&mut text)
                .is_err()
                || text.len() > 65_536
            {
                continue;
            }
            let json =
                serde_json::from_str::<serde_json::Value>(&crate::jsonc::strip_jsonc(&text)).ok();
            if let Some(setting) = read(name, json.as_ref(), &text) {
                return Some(setting);
            }
        }
        if dir == root {
            break;
        }
        dir = dir.parent()?;
    }
    None
}

pub fn module_literal(module: &str, quote: char) -> String {
    let mut literal = String::new();
    literal.push(quote);
    for ch in module.chars() {
        match ch {
            '\\' => literal.push_str("\\\\"),
            '\n' => literal.push_str("\\n"),
            '\r' => literal.push_str("\\r"),
            '\t' => literal.push_str("\\t"),
            '\u{2028}' => literal.push_str("\\u2028"),
            '\u{2029}' => literal.push_str("\\u2029"),
            ch if ch == quote => {
                literal.push('\\');
                literal.push(ch);
            }
            ch if ch.is_control() => literal.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => literal.push(ch),
        }
    }
    literal.push(quote);
    literal
}
