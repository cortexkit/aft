//! Per-file syntax facts persisted in content records and consumed by inspect.

use crate::calls::extract_type_references;
use crate::imports::{parse_imports, specifier_imported_name, specifier_local_name};
use crate::parser::LangId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub(crate) const RUST_MACRO_REF_SHAPE_CALL: &str = "call";
pub(crate) const RUST_MACRO_REF_SHAPE_METHOD: &str = "method";
pub(crate) const RUST_MACRO_REF_SHAPE_STRUCT: &str = "struct";
pub(crate) const TOP_LEVEL_SYMBOL: &str = "<top-level>";
#[derive(Debug, Default)]
pub(crate) struct FileAnalysis {
    pub(crate) oversized: bool,
    pub(crate) freshness: Option<crate::cache_freshness::FileFreshness>,
    pub(crate) rust_api: Option<RustApiFacts>,
    pub(crate) go_dispatch: Option<GoDispatchFacts>,
    pub(crate) raw_imports: Vec<RawImportContribution>,
    pub(crate) rust_imports: Vec<RawImportContribution>,
    pub(crate) raw_reexports: Vec<RawReexportContribution>,
    pub(crate) attribute_entry_points: Vec<String>,
    pub(crate) macro_token_refs: Vec<MacroTokenRefContribution>,
    pub(crate) cfg_test_ranges: Vec<RustCfgTestRange>,
    pub(crate) cfg_test_module_files: Vec<String>,
    pub(crate) globs_parent_module: bool,
    pub(crate) trait_impl_methods: Vec<String>,
    pub(crate) type_ref_names: BTreeSet<String>,
}

/// What dead-code analysis reads from one file's syntax tree, recorded
/// once per content when the data layer parses the file, so an analysis
/// reads it from the file's content record instead of parsing again.
///
/// Records are shared by every path with the same content, so nothing here
/// depends on the file's path: what does (the files of out-of-line modules,
/// a Go file's module) is completed for each path by
/// [`StoredFileFacts::into_analysis`].
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct StoredFileFacts {
    /// The source is not UTF-8 text, so analysis reads nothing from it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) not_text: bool,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(crate) type_ref_names: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) js: Option<StoredJsFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rust: Option<StoredRustFacts>,
    /// A Go file's dispatch facts, without its module.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) go: Option<GoDispatchFacts>,
}

/// A JavaScript or TypeScript file's imports and re-exports, read when the
/// Oxc engine has no facts for the file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct StoredJsFacts {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) raw_imports: Vec<RawImportContribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) raw_reexports: Vec<RawReexportContribution>,
}

/// A Rust file's facts.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct StoredRustFacts {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) imports: Vec<RawImportContribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) raw_reexports: Vec<RawReexportContribution>,
    /// Its API facts. A module declared out of line has a placeholder file
    /// until the file's path completes it.
    pub(crate) api: RustApiFacts,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) attribute_entry_points: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) macro_token_refs: Vec<MacroTokenRefContribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) cfg_test_ranges: Vec<RustCfgTestRange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) cfg_test_modules: Vec<RustCfgTestModule>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) globs_parent_module: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) trait_impl_methods: Vec<String>,
}

impl StoredFileFacts {
    /// Whether analysis reads anything from a file in `lang`.
    pub(crate) fn read_for(lang: LangId) -> bool {
        supports_type_refs(lang)
    }

    /// The facts of `source` in `lang`, encoded for a content record:
    /// `source` as stored (the bytes of the file) and `tree`, the tree the
    /// data layer parsed it to.
    pub(crate) fn encode_for_record(
        source: &[u8],
        lang: LangId,
        tree: &tree_sitter::Tree,
    ) -> Vec<u8> {
        let facts = match std::str::from_utf8(source) {
            Ok(source) => Self::extract(source, lang, Some(tree)),
            Err(_) => Self {
                not_text: true,
                ..Self::default()
            },
        };
        serde_json::to_vec(&facts).unwrap_or_default()
    }

    /// Decodes facts [`Self::encode_for_record`] wrote.
    ///
    /// # Errors
    ///
    /// When the bytes are not such facts.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|error| error.to_string())
    }

    /// The facts of `source` in `lang`, from `tree`, its syntax tree (`None`
    /// when parsing failed).
    pub(crate) fn extract(source: &str, lang: LangId, tree: Option<&tree_sitter::Tree>) -> Self {
        if !Self::read_for(lang) {
            return Self::default();
        }
        let type_ref_names = tree
            .map(|tree| extract_type_references(source, tree.root_node(), lang))
            .unwrap_or_default();
        // Oxc FileFacts are the raw TS/JS import/re-export/dynamic-import
        // facts; these are read only for a file Oxc has no facts for.
        let js = matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript).then(|| {
            StoredJsFacts {
                raw_imports: tree
                    .map(|tree| raw_imports_from_tree(source, tree, lang))
                    .unwrap_or_default(),
                raw_reexports: tree
                    .map(|tree| ts_raw_reexport_contributions(source, tree.root_node()))
                    .unwrap_or_default(),
            }
        });
        let rust = (lang == LangId::Rust).then(|| {
            // Module files depend on the file's path; they are completed
            // per path from the placeholder.
            let api = tree
                .map(|tree| rust_api_facts(source, tree.root_node(), ""))
                .unwrap_or_default();
            let attribute_entry_points = tree
                .map(|tree| {
                    let mut roots = BTreeSet::new();
                    for entry in
                        crate::parser::rust_attribute_entry_points(source, tree.root_node())
                    {
                        roots.insert(entry.name);
                        roots.insert(entry.scoped_name);
                    }
                    roots.extend(rust_serde_function_references(
                        source,
                        tree.root_node(),
                        api.items.as_slice(),
                    ));
                    roots.into_iter().collect()
                })
                .unwrap_or_default();
            StoredRustFacts {
                imports: tree
                    .map(|tree| rust_raw_import_contributions(source, tree))
                    .unwrap_or_default(),
                raw_reexports: rust_raw_reexport_contributions(source),
                attribute_entry_points,
                macro_token_refs: tree
                    .map(|tree| rust_macro_token_refs(source, tree.root_node()))
                    .unwrap_or_default(),
                cfg_test_ranges: tree
                    .map(|tree| rust_cfg_test_ranges(source, tree.root_node()))
                    .unwrap_or_default(),
                cfg_test_modules: tree
                    .map(|tree| rust_cfg_test_modules(source, tree.root_node()))
                    .unwrap_or_default(),
                globs_parent_module: tree
                    .is_some_and(|tree| rust_globs_parent_module(source, tree.root_node())),
                trait_impl_methods: tree
                    .map(|tree| rust_trait_impl_methods(source, tree.root_node()))
                    .unwrap_or_default(),
                api,
            }
        });
        let go = (lang == LangId::Go).then(|| {
            tree.map(|tree| go_dispatch_facts(source, tree.root_node()))
                .unwrap_or_default()
        });
        Self {
            not_text: false,
            type_ref_names,
            js,
            rust,
            go,
        }
    }

    /// The analysis of the file at `file` (`relative_file` from the project
    /// root), whose language is `lang`, from its facts. `has_oxc_file` says
    /// the Oxc engine has facts for it, which replace its raw imports and
    /// re-exports.
    pub(crate) fn into_analysis(
        self,
        lang: LangId,
        file: &Path,
        relative_file: &str,
        has_oxc_file: bool,
    ) -> FileAnalysis {
        if self.not_text || !Self::read_for(lang) {
            return FileAnalysis::default();
        }
        let needs_ts_raw_facts =
            matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) && !has_oxc_file;
        let js = self.js.unwrap_or_default();
        let rust = (lang == LangId::Rust).then(|| {
            let mut rust = self.rust.unwrap_or_default();
            for module in &mut rust.api.modules {
                if module.file.is_some() {
                    let mut nested = module.scope.clone();
                    nested.push(module.name.clone());
                    module.file = Some(rust_module_file(
                        relative_file,
                        &nested,
                        module.path_override.as_deref(),
                    ));
                }
            }
            rust
        });
        let go_dispatch = (lang == LangId::Go).then(|| {
            let mut facts = self.go.unwrap_or_default();
            facts.module = go_module_dir(file, relative_file);
            facts
        });
        let raw_reexports = if needs_ts_raw_facts {
            js.raw_reexports
        } else {
            rust.as_ref()
                .map(|rust| rust.raw_reexports.clone())
                .unwrap_or_default()
        };
        let Some(rust) = rust else {
            return FileAnalysis {
                go_dispatch,
                raw_imports: if needs_ts_raw_facts {
                    js.raw_imports
                } else {
                    Vec::new()
                },
                raw_reexports,
                type_ref_names: self.type_ref_names,
                ..FileAnalysis::default()
            };
        };
        FileAnalysis {
            oversized: false,
            freshness: None,
            go_dispatch,
            raw_imports: Vec::new(),
            raw_reexports,
            cfg_test_module_files: rust_cfg_test_module_files_for(
                relative_file,
                &rust.cfg_test_modules,
            ),
            rust_api: Some(rust.api),
            rust_imports: rust.imports,
            attribute_entry_points: rust.attribute_entry_points,
            macro_token_refs: rust.macro_token_refs,
            cfg_test_ranges: rust.cfg_test_ranges,
            globs_parent_module: rust.globs_parent_module,
            trait_impl_methods: rust.trait_impl_methods,
            type_ref_names: self.type_ref_names,
        }
    }
}

/// The directory of the Go module the file at `file` (`relative_file` from
/// the project root) belongs to, relative to the root: the nearest
/// directory above it holding a `go.mod`, or the root's own when none does.
pub(crate) fn go_module_dir(file: &Path, relative_file: &str) -> String {
    let mut module = String::new();
    let mut relative = Path::new(relative_file).parent().unwrap_or(Path::new(""));
    let mut absolute = file.parent().unwrap_or(Path::new(""));
    loop {
        if absolute.join("go.mod").is_file() {
            module = relative.to_string_lossy().replace('\\', "/");
            break;
        }
        let (Some(next_relative), Some(next_absolute)) = (relative.parent(), absolute.parent())
        else {
            break;
        };
        relative = next_relative;
        absolute = next_absolute;
    }
    module
}

#[derive(Debug, Clone)]
pub(crate) struct RustMacroToken<'a> {
    pub(crate) text: &'a str,
    pub(crate) kind: &'a str,
    pub(crate) line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RustCfgTestRange {
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
}

impl RustCfgTestRange {
    pub(crate) fn contains(self, line: u32) -> bool {
        self.start_line <= line && line <= self.end_line
    }
}

/// Whether the file has a file-level `use super::*;` (the import parser keeps
/// no record of glob imports, so this reads the syntax tree directly).
pub(crate) fn rust_globs_parent_module(source: &str, root: tree_sitter::Node) -> bool {
    let mut cursor = root.walk();
    let globs = root.children(&mut cursor).any(|node| {
        node.kind() == "use_declaration"
            && node_text(source, node)
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>()
                .ends_with("usesuper::*;")
    });
    globs
}

/// Functions named by serde field attributes (`deserialize_with = "f"`,
/// `serialize_with`, `default`, `skip_serializing_if`). serde calls them from
/// derived code the callgraph never sees, so they are liveness roots. Each
/// root is the declaration identity the path names from the attribute's
/// module, so `"parse"` inside `mod config` roots `config::parse` (see
/// `rust_serde_root_identity`).
pub(crate) fn rust_serde_function_references(
    source: &str,
    root: tree_sitter::Node,
    items: &[RustApiItem],
) -> Vec<String> {
    static SERDE_FUNCTION_ATTRIBUTE: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = SERDE_FUNCTION_ATTRIBUTE.get_or_init(|| {
        regex::Regex::new(
            r#"\b(?:deserialize_with|serialize_with|default|skip_serializing_if)\s*=\s*"([A-Za-z_][A-Za-z0-9_:]*)""#,
        )
        .expect("serde function attribute pattern compiles")
    });
    let identities = items
        .iter()
        .filter(|item| item.owner.is_none())
        .map(rust_qualified_item_name)
        .collect::<BTreeSet<_>>();
    pattern
        .captures_iter(source)
        .filter_map(|captures| captures.get(1))
        .map(|path| {
            let scope = root
                .descendant_for_byte_range(path.start(), path.end())
                .map(|node| rust_mod_scope_chain(&node, source))
                .unwrap_or_default();
            rust_serde_root_identity(path.as_str(), &scope, &identities)
        })
        .collect()
}

/// The declaration identity a serde callback path names from module `scope`.
/// `helpers::parse` inside `mod config` is `config::helpers::parse`; a leading
/// `self`, `super` or `crate` fixes the starting module. A relative path that
/// is not declared in `scope` is looked up in each enclosing module in turn,
/// as a `use super::...` import usually brings it from there, and a bare name
/// declared exactly once in the file resolves to that declaration. A path
/// that names no declaration in this file keeps only its last segment.
pub(crate) fn rust_serde_root_identity(
    path: &str,
    scope: &[String],
    identities: &BTreeSet<String>,
) -> String {
    let mut segments = rust_path_segments(path);
    let short = segments.last().cloned().unwrap_or_default();
    let mut scope = scope.to_vec();
    let mut anchored = false;
    while let Some(first) = segments.first().map(String::as_str) {
        match first {
            "self" => {}
            "super" => {
                scope.pop();
            }
            "crate" => scope.clear(),
            _ => break,
        }
        segments.remove(0);
        anchored = true;
    }
    loop {
        let candidate = scope
            .iter()
            .chain(&segments)
            .cloned()
            .collect::<Vec<_>>()
            .join("::");
        if identities.contains(&candidate) {
            return candidate;
        }
        if anchored || scope.pop().is_none() {
            break;
        }
    }
    if !anchored && segments.len() == 1 {
        let suffix = format!("::{short}");
        let mut matches = identities
            .iter()
            .filter(|identity| identity.ends_with(&suffix));
        if let (Some(only), None) = (matches.next(), matches.next()) {
            return only.clone();
        }
    }
    short
}

/// Names of the methods defined inside `impl Trait for Type` blocks anywhere
/// in the file (including inline modules).
pub(crate) fn rust_trait_impl_methods(source: &str, root: tree_sitter::Node) -> Vec<String> {
    let mut names = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "impl_item" && node.child_by_field_name("trait").is_some() {
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for item in body.children(&mut cursor) {
                    if item.kind() == "function_item" {
                        if let Some(name) = item.child_by_field_name("name") {
                            names.insert(node_text(source, name).to_string());
                        }
                    }
                }
            }
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    names.into_iter().collect()
}

pub(crate) fn rust_path_segments(path: &str) -> Vec<String> {
    path.split("::")
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn macro_scoped_symbol(path: &[String], name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{}::{name}", path.join("::"))
    }
}

pub(crate) fn raw_imports_from_tree(
    source: &str,
    tree: &tree_sitter::Tree,
    lang: LangId,
) -> Vec<RawImportContribution> {
    parse_imports(source, tree, lang)
        .imports
        .into_iter()
        .map(|import| RawImportContribution {
            source: import.module_path,
            names: import.names,
            default_import: import.default_import,
            namespace_import: import.namespace_import,
        })
        .collect()
}

pub(crate) fn rust_raw_import_contributions(
    source: &str,
    tree: &tree_sitter::Tree,
) -> Vec<RawImportContribution> {
    parse_imports(source, tree, LangId::Rust)
        .imports
        .into_iter()
        .map(|import| RawImportContribution {
            source: crate::imports::rust_use_tree(&import),
            names: import.names,
            default_import: None,
            namespace_import: None,
        })
        .collect()
}

/// Line ranges compiled only for tests: `#[cfg(test)]` items (modules,
/// functions, impls, types, traits, constants and statics), plus
/// `#[cfg(test)]` statements, blocks and nested items inside a function body.
/// Calls on these lines are test usage, and declarations on them are test
/// code.
pub(crate) fn rust_cfg_test_ranges(source: &str, root: tree_sitter::Node) -> Vec<RustCfgTestRange> {
    let mut ranges = Vec::new();
    // Each node with whether its parent is a block. The walk carries this
    // down rather than asking `Node::parent`, which searches from the root
    // and is quadratic over a file of many top-level nodes.
    let mut stack = vec![(root, false)];
    while let Some((node, parent_is_block)) = stack.pop() {
        let in_function_body = parent_is_block
            && !matches!(
                node.kind(),
                "attribute_item" | "inner_attribute_item" | "line_comment" | "block_comment"
            );
        let item = matches!(
            node.kind(),
            "mod_item"
                | "function_item"
                | "impl_item"
                | "struct_item"
                | "enum_item"
                | "union_item"
                | "trait_item"
                | "type_item"
                | "const_item"
                | "static_item"
        );
        if (in_function_body || item) && rust_node_has_cfg_test_attribute(source, node) {
            ranges.push(RustCfgTestRange {
                // Symbol ranges include preceding attributes, not just the fn
                // token. Include them so declaration and call classification
                // agree on a cfg-gated item's first line.
                start_line: rust_item_start_line(node),
                end_line: node.end_position().row as u32 + 1,
            });
        }

        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            let is_block = node.kind() == "block";
            loop {
                stack.push((cursor.node(), is_block));
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    ranges.sort_by_key(|range| (range.start_line, range.end_line));
    ranges.dedup();
    ranges
}

pub(crate) fn rust_item_start_line(node: tree_sitter::Node<'_>) -> u32 {
    let mut first = node;
    while let Some(previous) = first.prev_sibling() {
        if !matches!(
            previous.kind(),
            "attribute_item" | "line_comment" | "block_comment"
        ) {
            break;
        }
        first = previous;
    }
    first.start_position().row as u32 + 1
}

/// An out-of-line module a file declares under `#[cfg(test)]`, by the
/// `#[path]` it gives or by its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RustCfgTestModule {
    Path(String),
    Name(String),
}

/// The file-level `#[cfg(test)]` out-of-line module declarations of a
/// file, in source order.
pub(crate) fn rust_cfg_test_modules(
    source: &str,
    root: tree_sitter::Node,
) -> Vec<RustCfgTestModule> {
    let mut modules = Vec::new();
    let mut cursor = root.walk();
    for node in root.children(&mut cursor) {
        // Only file-level declarations: nested inline modules change the base
        // directory, and test-only nested declarations are rare.
        if node.kind() != "mod_item"
            || node.child_by_field_name("body").is_some()
            || !rust_node_has_cfg_test_attribute(source, node)
        {
            continue;
        }
        if let Some(path) = rust_path_attribute_value(source, node) {
            modules.push(RustCfgTestModule::Path(path));
            continue;
        }
        let Some(name) = node
            .child_by_field_name("name")
            .map(|name| node_text(source, name).to_string())
        else {
            continue;
        };
        modules.push(RustCfgTestModule::Name(name));
    }
    modules
}

/// The project-relative candidate files of `modules`, declared in the file
/// at `relative_file`.
pub(crate) fn rust_cfg_test_module_files_for(
    relative_file: &str,
    modules: &[RustCfgTestModule],
) -> Vec<String> {
    let normalized = relative_file.replace('\\', "/");
    let (dir, file_name) = normalized
        .rsplit_once('/')
        .map_or(("", normalized.as_str()), |(dir, name)| (dir, name));
    // `mod x;` in `mod.rs`, `lib.rs` or `main.rs` resolves next to the file;
    // in `foo.rs` it resolves under `foo/`.
    let module_dir = match file_name.strip_suffix(".rs") {
        Some("mod" | "lib" | "main") | None => dir.to_string(),
        Some(stem) if dir.is_empty() => stem.to_string(),
        Some(stem) => format!("{dir}/{stem}"),
    };
    let join = |base: &str, rest: &str| {
        if base.is_empty() {
            rest.to_string()
        } else {
            format!("{base}/{rest}")
        }
    };

    let mut files = Vec::new();
    for module in modules {
        match module {
            RustCfgTestModule::Path(path) => {
                // A `#[path]` on a file-level declaration is relative to the
                // directory of the declaring file.
                files.push(normalize_relative_segments(&join(dir, path)));
            }
            RustCfgTestModule::Name(name) => {
                files.push(join(&module_dir, &format!("{name}.rs")));
                files.push(join(&module_dir, &format!("{name}/mod.rs")));
            }
        }
    }
    files
}

/// The string value of a `#[path = "..."]` attribute attached to `node`.
pub(crate) fn rust_path_attribute_value(
    source: &str,
    node: tree_sitter::Node<'_>,
) -> Option<String> {
    let mut previous = node.prev_sibling();
    while let Some(attribute) = previous {
        match attribute.kind() {
            "attribute_item" => {
                let text = &source[attribute.byte_range()];
                let inner = text
                    .trim()
                    .strip_prefix("#[")
                    .and_then(|inner| inner.strip_suffix(']'))
                    .map(str::trim);
                if let Some(value) = inner
                    .and_then(|inner| inner.strip_prefix("path"))
                    .map(str::trim_start)
                    .and_then(|rest| rest.strip_prefix('='))
                    .map(str::trim)
                    .and_then(|rest| rest.strip_prefix('"'))
                    .and_then(|rest| rest.strip_suffix('"'))
                {
                    return Some(value.replace('\\', "/"));
                }
                previous = attribute.prev_sibling();
            }
            "line_comment" | "block_comment" => previous = attribute.prev_sibling(),
            _ => break,
        }
    }
    None
}

/// Resolves `.` and `..` segments of a project-relative path textually.
pub(crate) fn normalize_relative_segments(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    // Joined PathBufs use backslashes on Windows, but contribution keys always
    // use forward slashes. Normalize before resolving dot segments.
    for segment in path.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

pub(crate) fn rust_node_has_cfg_test_attribute(source: &str, node: tree_sitter::Node<'_>) -> bool {
    let mut previous = node.prev_sibling();
    while let Some(attribute) = previous {
        match attribute.kind() {
            "attribute_item" => {
                if rust_attribute_requires_test(source, attribute) {
                    return true;
                }
                previous = attribute.prev_sibling();
            }
            "line_comment" | "block_comment" => previous = attribute.prev_sibling(),
            _ => break,
        }
    }
    false
}

/// `parent`'s named children, each with whether
/// [`rust_node_has_cfg_test_attribute`] holds for it, worked out in one pass
/// over the children. Asking each child separately walks back over every
/// comment before it, which is quadratic in a file of many comment lines.
pub(crate) fn rust_children_cfg_test_gated<'t>(
    source: &str,
    parent: tree_sitter::Node<'t>,
) -> Vec<(tree_sitter::Node<'t>, bool)> {
    let mut children = Vec::new();
    // Whether an attribute requiring `test` is in the run of attributes and
    // comments right before the current child.
    let mut run_requires_test = false;
    let mut cursor = parent.walk();
    for node in parent.children(&mut cursor) {
        if node.is_named() {
            children.push((node, run_requires_test));
        }
        match node.kind() {
            "attribute_item" => {
                run_requires_test = run_requires_test || rust_attribute_requires_test(source, node);
            }
            "line_comment" | "block_comment" => {}
            _ => run_requires_test = false,
        }
    }
    children
}

/// Whether `attribute` is `#[cfg(...)]` with a predicate requiring `test`.
pub(crate) fn rust_attribute_requires_test(source: &str, attribute: tree_sitter::Node<'_>) -> bool {
    let compact = source[attribute.byte_range()]
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    compact
        .strip_prefix("#[cfg(")
        .and_then(|inner| inner.strip_suffix(")]"))
        .is_some_and(cfg_predicate_requires_test)
}

pub(crate) fn cfg_predicate_requires_test(predicate: &str) -> bool {
    if predicate == "test" {
        return true;
    }
    if let Some(inner) = predicate
        .strip_prefix("all(")
        .and_then(|inner| inner.strip_suffix(')'))
    {
        return split_cfg_predicates(inner)
            .into_iter()
            .any(cfg_predicate_requires_test);
    }
    if let Some(inner) = predicate
        .strip_prefix("any(")
        .and_then(|inner| inner.strip_suffix(')'))
    {
        let predicates = split_cfg_predicates(inner);
        return !predicates.is_empty() && predicates.into_iter().all(cfg_predicate_requires_test);
    }
    false
}

pub(crate) fn split_cfg_predicates(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(input[start..index].trim());
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    let tail = input[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

pub(crate) fn rust_macro_token_refs(
    source: &str,
    root: tree_sitter::Node,
) -> Vec<MacroTokenRefContribution> {
    let mut refs = BTreeSet::new();
    let mut scope_stack = Vec::new();
    collect_rust_macro_token_refs(source, root, &mut scope_stack, &mut refs);
    // Preserve lexical paths and owners even when the stored callgraph uses a
    // short name. This also retains every cfg-alternative function body.
    let mut functions: BTreeMap<String, Vec<tree_sitter::Node>> = BTreeMap::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "function_item" {
            if let Some(name) = rust_function_symbol_name(source, &node) {
                functions.entry(name).or_default().push(node);
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    for (caller_symbol, variants) in functions {
        for node in variants {
            for (full, _, line, _, _) in rust_function_body_calls(source, node) {
                if full.contains('.') || full.ends_with('!') {
                    continue;
                }
                let mut path = rust_path_segments(&full);
                let Some(name) = path.pop() else {
                    continue;
                };
                refs.insert(MacroTokenRefContribution {
                    caller_symbol: caller_symbol.clone(),
                    name,
                    line,
                    path: macro_ref_path(path),
                    shape: RUST_MACRO_REF_SHAPE_CALL.into(),
                });
            }
        }
    }
    refs.into_iter().collect()
}

/// Calls that run when this Rust function body runs.
///
/// A call written in a `macro_rules!` template that is defined inside the body
/// runs only where that macro is expanded, not where it is defined. Such a
/// call therefore counts only when the body invokes the macro (`name!(..)`),
/// directly or through another local macro whose template is itself expanded.
/// A local macro that nothing invokes contributes no calls. An invocation in a
/// nested `fn` item counts for the enclosing body, as the stored call graph
/// also attributes a nested item's calls to the function around it.
pub(crate) fn rust_function_body_calls(
    source: &str,
    function: tree_sitter::Node,
) -> Vec<crate::calls::CallTuple> {
    let (calls, facts) = crate::calls::extract_rust_calls_with_macro_facts(
        source,
        function,
        function.start_byte(),
        function.end_byte(),
    );
    if facts.bodies.is_empty() {
        return calls;
    }

    // Templates can nest; the innermost one holding a call owns it.
    let owner = |start: usize, end: usize| {
        facts
            .bodies
            .iter()
            .filter(|body| body.byte_start <= start && end <= body.byte_end)
            .min_by_key(|body| body.byte_end - body.byte_start)
            .map(|body| body.name.as_str())
    };

    let mut direct_invocations = Vec::new();
    let mut template_invocations: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (full, _, _, start, end) in &calls {
        let Some(path) = full.strip_suffix('!') else {
            continue;
        };
        // A path-qualified invocation (`self::m!`) is matched by its last
        // segment: a same-named macro elsewhere may make an unused local
        // macro look invoked, which only keeps its calls live.
        let name = path.rsplit("::").next().unwrap_or(path).trim().to_string();
        match owner(*start, *end) {
            Some(macro_name) => template_invocations
                .entry(macro_name)
                .or_default()
                .push(name),
            None => direct_invocations.push(name),
        }
    }

    let mut invoked = BTreeSet::new();
    let mut pending = direct_invocations;
    while let Some(name) = pending.pop() {
        if !invoked.insert(name.clone()) {
            continue;
        }
        if let Some(nested) = template_invocations.get(name.as_str()) {
            pending.extend(nested.iter().cloned());
        }
    }
    // An expanded template that invokes a macro through a metavariable
    // (`$m!(..)`) can name any local macro, and which one is decided only by
    // the arguments at each expansion. The scanner cannot resolve that, so
    // every template call in this body stays live: a function wrongly reported
    // dead costs more than a dead one that goes unreported. A template whose
    // token tree cannot be read (its node is not found, or tree-sitter could
    // not parse it cleanly) may hide such an invocation, so it is treated the
    // same way and keeps the calls live.
    let unresolved_invocation = facts.bodies.iter().any(|body| {
        invoked.contains(&body.name)
            && rust_template_token_tree(function, body.byte_start - 1, body.byte_end + 1)
                .is_none_or(|tree| {
                    tree.has_error() || rust_token_tree_invokes_metavariable_macro(tree)
                })
    });
    if unresolved_invocation {
        return calls;
    }
    calls
        .into_iter()
        .filter(|(_, _, _, start, end)| {
            owner(*start, *end).is_none_or(|macro_name| invoked.contains(macro_name))
        })
        .collect()
}

/// The `token_tree` node spanning exactly `start..end`, including its
/// delimiters. Templates nested in another template are token trees of the
/// outer one, so they are found the same way.
pub(crate) fn rust_template_token_tree(
    root: tree_sitter::Node<'_>,
    start: usize,
    end: usize,
) -> Option<tree_sitter::Node<'_>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let range = node.byte_range();
        if range.start > start || range.end < end {
            continue;
        }
        if node.kind() == "token_tree" && range.start == start && range.end == end {
            return Some(node);
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    None
}

/// Whether a template token tree, at any depth, invokes a macro named by a
/// metavariable: a `$name` token followed by `!`, with comments and whitespace
/// allowed in between, as in `$callback /* note */ !(..)`.
pub(crate) fn rust_token_tree_invokes_metavariable_macro(
    token_tree: tree_sitter::Node<'_>,
) -> bool {
    let mut stack = vec![token_tree];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        // Comments are tree-sitter extras; dropping them lets the `!` after a
        // comment sit next to the metavariable it follows.
        let tokens = node
            .children(&mut cursor)
            .filter(|child| !child.is_extra())
            .collect::<Vec<_>>();
        for (index, token) in tokens.iter().enumerate() {
            if token.child_count() > 0 {
                stack.push(*token);
            }
            if token.kind() != "metavariable" {
                continue;
            }
            // The grammar's metavariable token covers only an ASCII name, so a
            // name like `$mé` continues as identifier tokens that start exactly
            // where the previous token ends.
            let mut next = index + 1;
            let mut name_end = token.end_byte();
            while let Some(part) = tokens
                .get(next)
                .filter(|part| part.kind() == "identifier" && part.start_byte() == name_end)
            {
                name_end = part.end_byte();
                next += 1;
            }
            if tokens.get(next).is_some_and(|bang| bang.kind() == "!") {
                return true;
            }
        }
    }
    false
}

pub(crate) fn collect_rust_macro_token_refs(
    source: &str,
    node: tree_sitter::Node,
    scope_stack: &mut Vec<String>,
    refs: &mut BTreeSet<MacroTokenRefContribution>,
) {
    let scope_len = scope_stack.len();
    if node.kind() == "function_item" {
        if let Some(symbol) = rust_function_symbol_name(source, &node) {
            scope_stack.push(symbol);
        }
    }

    if node.kind() == "macro_invocation" {
        if let Some(token_tree) = find_child_by_kind(node, "token_tree") {
            let caller_symbol = scope_stack
                .last()
                .cloned()
                .unwrap_or_else(|| TOP_LEVEL_SYMBOL.to_string());
            let mut tokens = Vec::new();
            collect_rust_macro_tokens(source, token_tree, &mut tokens);
            let mut shadowed = BTreeSet::new();
            for token in &tokens {
                if rust_macro_token_is_identifier(token)
                    && crate::calls::rust_name_is_shadowed_at(node, source, token.text)
                {
                    shadowed.insert(token.text.to_string());
                }
            }
            let mut binding = false;
            for token in &tokens {
                if token.text == "let" {
                    binding = true;
                } else if token.text == "=" {
                    binding = false;
                } else if binding && rust_macro_token_is_identifier(token) {
                    shadowed.insert(token.text.to_string());
                }
            }
            extract_rust_macro_token_refs(&tokens, &caller_symbol, &shadowed, refs);
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_rust_macro_token_refs(source, cursor.node(), scope_stack, refs);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    scope_stack.truncate(scope_len);
}

pub(crate) fn collect_rust_macro_tokens<'a>(
    source: &'a str,
    node: tree_sitter::Node,
    tokens: &mut Vec<RustMacroToken<'a>>,
) {
    if rust_macro_token_node_is_opaque(node.kind()) {
        return;
    }

    if node.child_count() == 0 {
        let text = node_text(source, node).trim();
        if !text.is_empty() {
            tokens.push(RustMacroToken {
                text,
                kind: node.kind(),
                line: node.start_position().row as u32 + 1,
            });
        }
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_rust_macro_tokens(source, cursor.node(), tokens);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

pub(crate) fn rust_macro_token_node_is_opaque(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal" | "raw_string_literal" | "char_literal" | "line_comment" | "block_comment"
    )
}

pub(crate) fn extract_rust_macro_token_refs(
    tokens: &[RustMacroToken<'_>],
    caller_symbol: &str,
    shadowed: &BTreeSet<String>,
    refs: &mut BTreeSet<MacroTokenRefContribution>,
) {
    for index in 0..tokens.len() {
        let token = &tokens[index];
        if !rust_macro_token_is_identifier(token) || rust_macro_token_is_keyword(token.text) {
            continue;
        }
        if index > 0 && tokens[index - 1].text == "." {
            continue;
        }
        if tokens.get(index + 1).is_some_and(|next| next.text == "!") {
            continue;
        }

        let path = rust_macro_path_before(tokens, index);
        let next = rust_macro_next_after_optional_turbofish(tokens, index + 1);
        // A single bare argument can be a function item passed to a method
        // such as is_ok_and. Resolve it only locally or through imports, and
        // never credit a name bound by the enclosing function or macro body.
        if index > 0
            && tokens[index - 1].text == "("
            && tokens
                .get(next)
                .is_some_and(|next| matches!(next.text, ")" | ","))
            && !shadowed.contains(token.text)
        {
            refs.insert(MacroTokenRefContribution {
                caller_symbol: caller_symbol.to_string(),
                line: token.line,
                name: token.text.to_string(),
                path: macro_ref_path(path.clone()),
                shape: RUST_MACRO_REF_SHAPE_CALL.into(),
            });
        }
        if tokens.get(next).is_some_and(|next| next.text == "(") {
            let shape = if path
                .last()
                .is_some_and(|segment| rust_macro_name_is_upper_camel(segment))
            {
                RUST_MACRO_REF_SHAPE_METHOD
            } else {
                RUST_MACRO_REF_SHAPE_CALL
            };
            refs.insert(MacroTokenRefContribution {
                caller_symbol: caller_symbol.to_string(),
                line: token.line,
                name: token.text.to_string(),
                path: macro_ref_path(path),
                shape: shape.to_string(),
            });
            continue;
        }

        if rust_macro_name_is_upper_camel(token.text)
            && tokens.get(index + 1).is_some_and(|next| next.text == "{")
        {
            refs.insert(MacroTokenRefContribution {
                caller_symbol: caller_symbol.to_string(),
                line: token.line,
                name: token.text.to_string(),
                path: macro_ref_path(path),
                shape: RUST_MACRO_REF_SHAPE_STRUCT.to_string(),
            });
        }
    }
}

pub(crate) fn rust_macro_path_before(tokens: &[RustMacroToken<'_>], index: usize) -> Vec<String> {
    let mut segments = Vec::new();
    let mut cursor = index;
    while cursor >= 2
        && tokens[cursor - 1].text == "::"
        && rust_macro_token_is_path_segment(&tokens[cursor - 2])
    {
        segments.push(tokens[cursor - 2].text.to_string());
        cursor -= 2;
    }
    segments.reverse();
    segments
}

pub(crate) fn rust_macro_next_after_optional_turbofish(
    tokens: &[RustMacroToken<'_>],
    index: usize,
) -> usize {
    if tokens.get(index).is_none_or(|token| token.text != "::")
        || tokens.get(index + 1).is_none_or(|token| token.text != "<")
    {
        return index;
    }

    let mut depth = 0usize;
    let mut cursor = index + 1;
    while let Some(token) = tokens.get(cursor) {
        match token.text {
            "<" => depth += 1,
            ">" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return cursor + 1;
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    index
}

pub(crate) fn macro_ref_path(path: Vec<String>) -> Option<Vec<String>> {
    (!path.is_empty()).then_some(path)
}

pub(crate) fn rust_macro_token_is_identifier(token: &RustMacroToken<'_>) -> bool {
    matches!(token.kind, "identifier" | "type_identifier")
        || rust_macro_text_is_identifier(token.text)
}

pub(crate) fn rust_macro_token_is_path_segment(token: &RustMacroToken<'_>) -> bool {
    rust_macro_token_is_identifier(token)
        && (!rust_macro_token_is_keyword(token.text)
            || matches!(token.text, "crate" | "self" | "super"))
}

pub(crate) fn rust_macro_text_is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

pub(crate) fn rust_macro_name_is_upper_camel(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

pub(crate) fn rust_macro_token_is_keyword(text: &str) -> bool {
    matches!(
        text,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
    )
}

pub(crate) fn rust_function_symbol_name(
    source: &str,
    function_node: &tree_sitter::Node<'_>,
) -> Option<String> {
    let name_node = function_node.child_by_field_name("name")?;
    let name = node_text(source, name_node).to_string();
    let declaration_list_owner = rust_function_declaration_list_owner(function_node);

    match declaration_list_owner.as_ref().map(tree_sitter::Node::kind) {
        Some("impl_item") => {
            let scope_name = rust_impl_scope_name(declaration_list_owner.as_ref().unwrap(), source);
            if scope_name.is_empty() {
                Some(name)
            } else {
                Some(format!("{scope_name}::{name}"))
            }
        }
        Some(owner_kind) if owner_kind != "mod_item" => None,
        _ => {
            let scope_chain = rust_mod_scope_chain(function_node, source);
            if scope_chain.is_empty() {
                Some(name)
            } else {
                Some(format!("{}::{name}", scope_chain.join("::")))
            }
        }
    }
}

pub(crate) fn rust_function_declaration_list_owner<'a>(
    function_node: &tree_sitter::Node<'a>,
) -> Option<tree_sitter::Node<'a>> {
    function_node
        .parent()
        .filter(|parent| parent.kind() == "declaration_list")
        .and_then(|parent| parent.parent())
}

pub(crate) fn rust_mod_scope_chain(node: &tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let mut scopes = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "mod_item" {
            if let Some(name_node) = parent.child_by_field_name("name") {
                scopes.push(node_text(source, name_node).to_string());
            }
        }
        current = parent.parent();
    }
    scopes.reverse();
    scopes
}

pub(crate) fn rust_impl_scope_name(impl_node: &tree_sitter::Node<'_>, source: &str) -> String {
    let Some(self_type) = impl_node.child_by_field_name("type") else {
        return String::new();
    };
    let self_name = node_text(source, self_type);
    match impl_node.child_by_field_name("trait") {
        Some(trait_type) => format!("{} for {self_name}", node_text(source, trait_type)),
        None => self_name.to_string(),
    }
}

pub(crate) fn ts_raw_reexport_contributions(
    source: &str,
    root: tree_sitter::Node,
) -> Vec<RawReexportContribution> {
    let mut reexports = Vec::new();
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return reexports;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "export_statement" {
            if let Some(module_path) = export_source_module(source, node) {
                let line = (node.start_position().row + 1) as u32;
                let raw_export = node_text(source, node).trim();
                for specifier in ts_reexport_specifiers(raw_export) {
                    reexports.push(RawReexportContribution {
                        language: "ts".to_string(),
                        source: module_path.clone(),
                        kind: "named".to_string(),
                        imported: Some(specifier.imported),
                        exported: Some(specifier.exported),
                        line,
                    });
                }
                if raw_export.contains('*') {
                    if let Some(namespace_export) = ts_namespace_reexport_name(raw_export) {
                        reexports.push(RawReexportContribution {
                            language: "ts".to_string(),
                            source: module_path.clone(),
                            kind: "namespace".to_string(),
                            imported: Some("*".to_string()),
                            exported: Some(namespace_export),
                            line,
                        });
                    } else {
                        reexports.push(RawReexportContribution {
                            language: "ts".to_string(),
                            source: module_path.clone(),
                            kind: "star".to_string(),
                            imported: Some("*".to_string()),
                            exported: None,
                            line,
                        });
                    }
                }
            }
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    reexports
}

pub(crate) fn rust_raw_reexport_contributions(source: &str) -> Vec<RawReexportContribution> {
    rust_pub_use_statements(source)
        .into_iter()
        .flat_map(|(statement, line)| {
            rust_reexport_specifiers(&statement)
                .into_iter()
                .map(move |specifier| RawReexportContribution {
                    language: "rust".to_string(),
                    source: specifier.module_path.join("::"),
                    kind: if specifier.imported == "*" {
                        "star".to_string()
                    } else {
                        "named".to_string()
                    },
                    imported: Some(specifier.imported),
                    exported: Some(specifier.exported),
                    line,
                })
        })
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct RustApiFacts {
    #[serde(default)]
    pub(crate) receiver_references: BTreeSet<String>,
    #[serde(default)]
    pub(crate) initializers: Vec<RustInitializer>,
    #[serde(default)]
    pub(crate) initializer_references: Vec<MacroTokenRefContribution>,
    pub(crate) items: Vec<RustApiItem>,
    pub(crate) modules: Vec<RustApiModule>,
    pub(crate) uses: Vec<RustApiUse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RustInitializer {
    pub(crate) name: String,
    pub(crate) range: RustCfgTestRange,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RustApiItem {
    pub(crate) name: String,
    pub(crate) scope: Vec<String>,
    pub(crate) public: bool,
    pub(crate) line: u32,
    pub(crate) owner: Option<String>,
    pub(crate) trait_name: Option<String>,
}

pub(crate) fn rust_qualified_item_name(item: &RustApiItem) -> String {
    let mut parts = item.scope.clone();
    if let Some(owner) = &item.owner {
        parts.push(match &item.trait_name {
            Some(trait_name) => format!("{trait_name} for {owner}"),
            None => owner.clone(),
        });
    }
    parts.push(item.name.clone());
    parts.join("::")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RustApiModule {
    pub(crate) scope: Vec<String>,
    pub(crate) name: String,
    pub(crate) public: bool,
    pub(crate) file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) path_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RustApiUse {
    pub(crate) scope: Vec<String>,
    pub(crate) path: Vec<String>,
    pub(crate) alias: String,
    pub(crate) public: bool,
    pub(crate) line: u32,
}

pub(crate) fn rust_unrestricted_pub(source: &str, node: tree_sitter::Node) -> bool {
    find_child_by_kind(node, "visibility_modifier")
        .is_some_and(|visibility| node_text(source, visibility).trim() == "pub")
}

pub(crate) fn rust_api_facts(source: &str, root: tree_sitter::Node, file: &str) -> RustApiFacts {
    let mut facts = RustApiFacts::default();
    rust_collect_api_facts(source, root, file, &[], &mut facts);
    let mut owners = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let owner = match node.kind() {
            "function_item" => rust_function_symbol_name(source, &node),
            "static_item" | "const_item" => node.child_by_field_name("name").map(|name| {
                macro_scoped_symbol(
                    &rust_mod_scope_chain(&node, source),
                    node_text(source, name),
                )
            }),
            _ => None,
        };
        if let Some(owner) = owner {
            owners.push((node.start_byte(), node.end_byte(), owner.clone()));
            if matches!(node.kind(), "static_item" | "const_item") {
                facts.initializers.push(RustInitializer {
                    name: owner,
                    range: RustCfgTestRange {
                        start_line: rust_item_start_line(node),
                        end_line: node.end_position().row as u32 + 1,
                    },
                });
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    facts.initializer_references = crate::calls::extract_rust_value_references(source, root)
        .into_iter()
        .map(|(full, name, line, start, end)| {
            let mut path = rust_path_segments(&full);
            path.pop();
            let caller_symbol = owners
                .iter()
                .filter(|(a, b, _)| *a <= start && *b >= end)
                .min_by_key(|(a, b, _)| b - a)
                .map(|(_, _, name)| name.clone())
                .unwrap_or_else(|| "<top-level>".into());
            MacroTokenRefContribution {
                caller_symbol,
                name,
                line,
                path: macro_ref_path(path),
                shape: "call".into(),
            }
        })
        .collect();
    facts
}

pub(crate) fn rust_collect_api_facts(
    source: &str,
    root: tree_sitter::Node,
    file: &str,
    scope: &[String],
    facts: &mut RustApiFacts,
) {
    let gated = rust_children_cfg_test_gated(source, root);
    for (node, gated) in gated {
        if gated {
            continue;
        }
        // An impl header names its own receiver even when nothing uses it.
        // Only references outside impls are evidence of a runtime receiver.
        if !matches!(node.kind(), "impl_item" | "mod_item") {
            facts
                .receiver_references
                .extend(extract_type_references(source, node, LangId::Rust));
        }
        let name = node
            .child_by_field_name("name")
            .map(|name| node_text(source, name).to_string());
        let public = rust_unrestricted_pub(source, node);
        match node.kind() {
            "mod_item" => {
                let Some(name) = name else {
                    continue;
                };
                let body = node.child_by_field_name("body");
                let path_override = rust_path_attribute_value(source, node);
                let mut nested = scope.to_vec();
                nested.push(name.clone());
                let module_file = body
                    .is_none()
                    .then(|| rust_module_file(file, &nested, path_override.as_deref()));
                facts.modules.push(RustApiModule {
                    scope: scope.to_vec(),
                    name,
                    public,
                    file: module_file,
                    path_override,
                });
                if let Some(body) = body {
                    rust_collect_api_facts(source, body, file, &nested, facts);
                }
            }
            "use_declaration" => {
                let text = node_text(source, node);
                let Some((_, path)) = text.split_once("use ") else {
                    continue;
                };
                rust_api_use_paths(
                    path.trim().trim_end_matches(';'),
                    &[],
                    &mut |path, alias| {
                        facts.uses.push(RustApiUse {
                            scope: scope.to_vec(),
                            path,
                            alias,
                            public,
                            line: node.start_position().row as u32 + 1,
                        });
                    },
                );
            }
            "impl_item" => {
                let owner = node
                    .child_by_field_name("type")
                    .map(|ty| rust_api_type_path(node_text(source, ty)));
                let trait_name = node
                    .child_by_field_name("trait")
                    .map(|ty| rust_api_type_path(node_text(source, ty)));
                if let Some(body) = node.child_by_field_name("body") {
                    let mut cursor = body.walk();
                    for method in body
                        .named_children(&mut cursor)
                        .filter(|item| item.kind() == "function_item")
                    {
                        if rust_node_has_cfg_test_attribute(source, method) {
                            continue;
                        }
                        if let Some(name) = method.child_by_field_name("name") {
                            facts.items.push(RustApiItem {
                                name: node_text(source, name).to_string(),
                                scope: scope.to_vec(),
                                public: rust_unrestricted_pub(source, method)
                                    || trait_name.is_some(),
                                line: method.start_position().row as u32 + 1,
                                owner: owner.clone(),
                                trait_name: trait_name.clone(),
                            });
                        }
                    }
                }
            }
            "function_item" | "struct_item" | "enum_item" | "trait_item" | "type_item"
            | "const_item" | "static_item" => {
                if let Some(name) = name {
                    facts.items.push(RustApiItem {
                        name,
                        scope: scope.to_vec(),
                        public,
                        line: node.start_position().row as u32 + 1,
                        owner: None,
                        trait_name: None,
                    });
                }
            }
            "macro_invocation" => {
                // Declarative item macros often pass Rust items as their input.
                // Parse those items rather than making the whole module public.
                if let Some(tokens) = find_child_by_kind(node, "token_tree") {
                    rust_collect_api_macro_items(source, tokens, file, scope, facts);
                }
                if let Some(name) = node.child_by_field_name("macro") {
                    let mut cursor = root.walk();
                    for definition in root
                        .named_children(&mut cursor)
                        .filter(|n| n.kind() == "macro_definition")
                    {
                        if definition
                            .child_by_field_name("name")
                            .is_some_and(|n| node_text(source, n) == node_text(source, name))
                        {
                            let mut cursor = definition.walk();
                            for rule in definition
                                .named_children(&mut cursor)
                                .filter(|n| n.kind() == "macro_rule")
                            {
                                if let (Some(left), Some(right), Some(input)) = (
                                    rule.child_by_field_name("left"),
                                    rule.child_by_field_name("right"),
                                    find_child_by_kind(node, "token_tree"),
                                ) {
                                    if let Some(expanded) = rust_api_macro_expansion(
                                        node_text(source, left),
                                        node_text(source, right),
                                        node_text(source, input),
                                    ) {
                                        rust_collect_api_macro_text(
                                            &expanded,
                                            right.start_position().row,
                                            file,
                                            scope,
                                            facts,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// The project-relative file of the out-of-line module at `nested` (its
/// module path inside the declaring file), declared in the file at `file`,
/// with `path_override` from its `#[path]` attribute.
pub(crate) fn rust_module_file(
    file: &str,
    nested: &[String],
    path_override: Option<&str>,
) -> String {
    let base = Path::new(file);
    let dir = base.parent().unwrap_or(Path::new(""));
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let dir = if matches!(stem, "lib" | "main" | "mod") {
        dir.to_path_buf()
    } else {
        dir.join(stem)
    };
    if let Some(path) = path_override {
        normalize_relative_segments(
            &base
                .parent()
                .unwrap_or(Path::new(""))
                .join(path)
                .to_string_lossy(),
        )
    } else {
        dir.join(nested.join("/"))
            .with_extension("rs")
            .to_string_lossy()
            .replace('\\', "/")
    }
}

pub(crate) fn rust_collect_api_macro_items(
    source: &str,
    tokens: tree_sitter::Node,
    file: &str,
    scope: &[String],
    facts: &mut RustApiFacts,
) {
    let text = node_text(source, tokens);
    if text.len() < 2 {
        return;
    }
    rust_collect_api_macro_text(
        &text[1..text.len() - 1],
        tokens.start_position().row,
        file,
        scope,
        facts,
    );
}

// Expand simple ident/path captures used by re-export macros. Repetition and
// procedural macros require compiler expansion; unresolved templates are not
// evidence that every item in their module is public.
pub(crate) fn rust_api_macro_expansion(
    pattern: &str,
    template: &str,
    input: &str,
) -> Option<String> {
    let interior = |text: &str| {
        text.get(1..text.len().checked_sub(1)?)
            .map(str::trim)
            .map(str::to_string)
    };
    let pattern = interior(pattern)?;
    let input = interior(input)?;
    let mut template = interior(template)?;
    if !pattern.contains('$') {
        return (pattern == input).then_some(template);
    }
    let patterns = split_cfg_predicates(&pattern);
    let inputs = split_cfg_predicates(&input);
    if patterns.len() != inputs.len() {
        return None;
    }
    for (pattern, input) in patterns.into_iter().zip(inputs) {
        let (name, kind) = pattern.trim().strip_prefix('$')?.split_once(':')?;
        if !matches!(kind.trim(), "ident" | "path")
            || !name.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
        {
            return None;
        }
        if !input
            .trim()
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == ':')
        {
            return None;
        }
        template = template.replace(&format!("${}", name.trim()), input.trim());
    }
    (!template.contains('$')).then_some(template)
}

pub(crate) fn rust_collect_api_macro_text(
    interior: &str,
    row: usize,
    file: &str,
    scope: &[String],
    facts: &mut RustApiFacts,
) {
    if interior.contains('$') {
        return;
    }
    let padded = format!("{}{}", "\n".repeat(row), interior);
    let mut analyzer = DeadCodeFileAnalyzer::default();
    if let Some(tree) = analyzer.parse_source(LangId::Rust, &padded) {
        if !tree.root_node().has_error() {
            rust_collect_api_facts(&padded, tree.root_node(), file, scope, facts);
        }
    }
}

pub(crate) fn rust_api_type_path(text: &str) -> String {
    text.split('<').next().unwrap_or(text).trim().to_string()
}

pub(crate) fn rust_api_use_paths(
    text: &str,
    prefix: &[String],
    emit: &mut impl FnMut(Vec<String>, String),
) {
    let text = text.trim();
    if let Some(open) = text.find('{') {
        let mut prefix = prefix.to_vec();
        prefix.extend(rust_path_segments(text[..open].trim_end_matches(':')));
        let inner = text[open + 1..].trim_end_matches('}');
        let mut depth = 0;
        let mut start = 0;
        for (index, ch) in inner.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                ',' if depth == 0 => {
                    rust_api_use_paths(&inner[start..index], &prefix, emit);
                    start = index + 1;
                }
                _ => {}
            }
        }
        rust_api_use_paths(&inner[start..], &prefix, emit);
    } else if !text.is_empty() {
        let (path, alias) = text
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((text, None));
        let mut segments = prefix.to_vec();
        segments.extend(rust_path_segments(path));
        if segments.last().is_some_and(|s| s == "self") {
            segments.pop();
        }
        if let Some(name) = alias.or_else(|| segments.last().map(String::as_str)) {
            emit(segments.clone(), name.to_string());
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct GoDispatchFacts {
    #[serde(default)]
    pub(crate) module: String,
    pub(crate) methods: Vec<GoMethodFact>,
    pub(crate) interfaces: Vec<Vec<String>>,
    pub(crate) used_types: BTreeSet<String>,
    #[serde(default)]
    pub(crate) live_methods: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GoMethodFact {
    pub(crate) receiver: String,
    pub(crate) name: String,
    pub(crate) signature: String,
}

pub(crate) fn go_dispatch_facts(source: &str, root: tree_sitter::Node) -> GoDispatchFacts {
    let mut facts = GoDispatchFacts::default();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "method_declaration" {
            if let (Some(receiver), Some(name)) = (
                node.child_by_field_name("receiver"),
                node.child_by_field_name("name"),
            ) {
                let mut cursor = receiver.walk();
                if let Some(parameter) = receiver.named_children(&mut cursor).next() {
                    if let Some(ty) = parameter.child_by_field_name("type") {
                        facts.methods.push(GoMethodFact {
                            receiver: node_text(source, ty).trim_start_matches('*').to_string(),
                            name: node_text(source, name).to_string(),
                            signature: go_method_signature(source, node),
                        });
                    }
                }
                let mut cursor = node.walk();
                stack.extend(
                    node.named_children(&mut cursor)
                        .filter(|child| child.id() != receiver.id()),
                );
                continue;
            }
        }
        if node.kind() == "interface_type" {
            let mut cursor = node.walk();
            let methods = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "method_elem")
                .map(|method| go_method_signature(source, method))
                .collect::<Vec<_>>();
            // Empty interfaces impose no method contract. Embedded/constraint
            // interfaces need their complete method set; never accept a partial
            // set as if it were the whole interface.
            let mut cursor = node.walk();
            if !methods.is_empty()
                && !node
                    .named_children(&mut cursor)
                    .any(|child| child.kind() == "type_elem")
            {
                facts.interfaces.push(methods);
            }
            continue;
        }
        if node.kind() == "type_identifier"
            && !node.parent().is_some_and(|parent| {
                parent.kind() == "type_spec"
                    && parent
                        .child_by_field_name("name")
                        .is_some_and(|name| name.id() == node.id())
            })
        {
            facts.used_types.insert(node_text(source, node).to_string());
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    facts
}

pub(crate) fn go_method_signature(source: &str, method: tree_sitter::Node) -> String {
    let name = method
        .child_by_field_name("name")
        .map(|name| node_text(source, name))
        .unwrap_or("");
    let parameters = method
        .child_by_field_name("parameters")
        .map(|node| go_signature_types(source, node))
        .unwrap_or_default();
    let result = method
        .child_by_field_name("result")
        .map(|node| go_signature_types(source, node))
        .unwrap_or_default();
    format!("{name}({parameters})({result})")
}

pub(crate) fn go_signature_types(source: &str, node: tree_sitter::Node) -> String {
    if node.kind() != "parameter_list" {
        return node_text(source, node)
            .split_whitespace()
            .collect::<String>();
    }
    let mut types = Vec::new();
    let mut cursor = node.walk();
    for parameter in node.named_children(&mut cursor) {
        let Some(ty) = parameter.child_by_field_name("type") else {
            continue;
        };
        let mut cursor = parameter.walk();
        let count = parameter
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "identifier")
            .count()
            .max(1);
        let ty = format!(
            "{}{}",
            if parameter.kind() == "variadic_parameter_declaration" {
                "..."
            } else {
                ""
            },
            node_text(source, ty).split_whitespace().collect::<String>()
        );
        types.extend(std::iter::repeat_n(ty, count));
    }
    types.join(",")
}

pub(crate) fn ts_reexport_specifiers(raw_export: &str) -> Vec<ReexportSpecifier> {
    let Some(start) = raw_export.find('{').map(|index| index + 1) else {
        return Vec::new();
    };
    let Some(end) = raw_export[start..].find('}').map(|index| start + index) else {
        return Vec::new();
    };

    raw_export[start..end]
        .split(',')
        .filter_map(|specifier| {
            let specifier = specifier.trim();
            if specifier.is_empty() {
                return None;
            }
            let imported = specifier_imported_name(specifier).trim();
            let exported = specifier_local_name(specifier).trim();
            if imported.is_empty() || exported.is_empty() {
                return None;
            }
            Some(ReexportSpecifier {
                imported: imported.to_string(),
                exported: exported.to_string(),
            })
        })
        .collect()
}

pub(crate) fn ts_namespace_reexport_name(raw_export: &str) -> Option<String> {
    let after_star = raw_export.split_once('*')?.1.trim_start();
    let after_as = after_star.strip_prefix("as")?.trim_start();
    let name = after_as
        .split_whitespace()
        .next()?
        .trim_matches(|ch: char| ch == '{' || ch == '}' || ch == ';' || ch == ',');
    (!name.is_empty()).then(|| name.to_string())
}

pub(crate) fn rust_pub_use_statements(source: &str) -> Vec<(String, u32)> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut start_line = 0u32;

    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if current.is_empty() {
            if !(trimmed.starts_with("pub use ") || trimmed.starts_with("pub(crate) use ")) {
                continue;
            }
            start_line = (index + 1) as u32;
        }

        current.push(' ');
        current.push_str(trimmed);
        if trimmed.ends_with(';') {
            statements.push((current.trim().to_string(), start_line));
            current.clear();
        }
    }

    statements
}

pub(crate) fn rust_reexport_specifiers(statement: &str) -> Vec<RustReexportSpecifier> {
    let statement = statement
        .trim()
        .trim_end_matches(';')
        .strip_prefix("pub(crate) use ")
        .or_else(|| {
            statement
                .trim()
                .trim_end_matches(';')
                .strip_prefix("pub use ")
        })
        .unwrap_or("")
        .trim();
    if statement.is_empty() {
        return Vec::new();
    }

    if let Some((module_path, grouped)) = statement.split_once("::{") {
        let grouped = grouped.trim_end_matches('}');
        return grouped
            .split(',')
            .filter_map(|specifier| rust_reexport_specifier(module_path.trim(), specifier.trim()))
            .collect();
    }

    let Some((module_path, imported)) = statement.rsplit_once("::") else {
        return Vec::new();
    };
    rust_reexport_specifier(module_path.trim(), imported.trim())
        .into_iter()
        .collect()
}

pub(crate) fn rust_reexport_specifier(
    module_path: &str,
    specifier: &str,
) -> Option<RustReexportSpecifier> {
    if specifier.is_empty() {
        return None;
    }
    let (imported, exported) = specifier
        .split_once(" as ")
        .map(|(imported, exported)| (imported.trim(), exported.trim()))
        .unwrap_or((specifier.trim(), specifier.trim()));
    if imported.is_empty() || exported.is_empty() {
        return None;
    }
    Some(RustReexportSpecifier {
        module_path: rust_normalize_module_path(module_path),
        imported: imported.to_string(),
        exported: exported.to_string(),
    })
}

pub(crate) fn rust_normalize_module_path(module_path: &str) -> Vec<String> {
    module_path
        .split("::")
        .filter_map(|segment| {
            let segment = segment.trim();
            if segment.is_empty() || matches!(segment, "self" | "crate") {
                None
            } else {
                Some(segment.to_string())
            }
        })
        .collect()
}

pub(crate) fn export_source_module(source: &str, node: tree_sitter::Node) -> Option<String> {
    node.child_by_field_name("source")
        .or_else(|| find_child_by_kind(node, "string"))
        .and_then(|source_node| string_literal_content(source, source_node))
}

pub(crate) fn find_child_by_kind<'tree>(
    node: tree_sitter::Node<'tree>,
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        let child = cursor.node();
        if child.kind() == kind {
            return Some(child);
        }
        if let Some(descendant) = find_child_by_kind(child, kind) {
            return Some(descendant);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    None
}

pub(crate) fn string_literal_content(source: &str, node: tree_sitter::Node) -> Option<String> {
    let raw = node_text(source, node).trim();
    let quote = raw.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    raw.strip_prefix(quote)
        .and_then(|value| value.strip_suffix(quote))
        .map(ToOwned::to_owned)
}

pub(crate) fn node_text<'a>(source: &'a str, node: tree_sitter::Node) -> &'a str {
    &source[node.byte_range()]
}

#[derive(Debug)]
pub(crate) struct ReexportSpecifier {
    pub(crate) imported: String,
    pub(crate) exported: String,
}

#[derive(Debug)]
pub(crate) struct RustReexportSpecifier {
    pub(crate) module_path: Vec<String>,
    pub(crate) imported: String,
    pub(crate) exported: String,
}

pub(crate) fn supports_type_refs(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::TypeScript
            | LangId::Tsx
            | LangId::JavaScript
            | LangId::Python
            | LangId::Rust
            | LangId::Go
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RawImportContribution {
    pub(crate) source: String,
    #[serde(default)]
    pub(crate) names: Vec<String>,
    #[serde(default)]
    pub(crate) default_import: Option<String>,
    #[serde(default)]
    pub(crate) namespace_import: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RawReexportContribution {
    pub(crate) language: String,
    pub(crate) source: String,
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) imported: Option<String>,
    #[serde(default)]
    pub(crate) exported: Option<String>,
    pub(crate) line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct MacroTokenRefContribution {
    pub(crate) caller_symbol: String,
    pub(crate) line: u32,
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<Vec<String>>,
    pub(crate) shape: String,
}
