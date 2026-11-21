use std::path::{Path, PathBuf};

use tree_sitter::{Node, Parser};

const UPPER: &[&str] = &[
    "subc",
    "commands",
    "cli",
    "run_tool_call",
    "subc_translate",
    "subc_format",
    "response_finalize",
    "bounded_io_command",
    "tool_output",
    "trace_measure_run",
];
const TRACE_FACADE: &str = "pub use crate::trace_measure_run::run;";

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    node.named_children(&mut node.walk()).collect()
}

fn attribute(node: Node<'_>) -> Option<Node<'_>> {
    node.named_child(0).filter(|it| it.kind() == "attribute")
}

// `all` needs only one test-only clause; `any` needs every alternative to be test-only.
fn requires_test(predicate: &str) -> bool {
    let predicate = predicate.trim();
    if predicate == "test" {
        return true;
    }
    let Some((combinator, rest)) = predicate.split_once('(') else {
        return false;
    };
    let Some(clauses) = rest.strip_suffix(')') else {
        return false;
    };
    let mut depth = 0;
    let mut start = 0;
    let mut checks = Vec::new();
    for (offset, ch) in clauses.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                checks.push(requires_test(&clauses[start..offset]));
                start = offset + 1;
            }
            _ => {}
        }
    }
    if !clauses[start..].trim().is_empty() {
        checks.push(requires_test(&clauses[start..]));
    }
    !checks.is_empty()
        && match combinator.trim() {
            "all" => checks.into_iter().any(|check| check),
            "any" => checks.into_iter().all(|check| check),
            _ => false,
        }
}

fn test_cfg(attr: Node<'_>, source: &str) -> bool {
    attr.named_child(0)
        .is_some_and(|name| text(name, source) == "cfg")
        && attr.child_by_field_name("arguments").is_some_and(|args| {
            let args = text(args, source);
            requires_test(&args[1..args.len() - 1])
        })
}

fn is_test_item(attrs: &[Node<'_>], node: Node<'_>, source: &str) -> bool {
    let inner = if matches!(node.kind(), "block" | "declaration_list" | "source_file") {
        Vec::new()
    } else {
        children(node)
    };
    attrs.iter().chain(inner.iter()).any(|it| {
        it.kind() == "attribute_item"
            && attribute(*it).is_some_and(|attr| {
                test_cfg(attr, source)
                    || (node.kind() == "function_item" && text(attr, source) == "test")
            })
    })
}

fn forbidden(path: &[&str]) -> bool {
    fn segment(part: &str) -> &str {
        part.strip_prefix("r#").unwrap_or(part)
    }
    path.windows(2).any(|pair| {
        let (owner, name) = (segment(pair[0]), segment(pair[1]));
        (owner == "trace_measure" && name == "run")
            || (matches!(owner, "crate" | "super" | "aft" | "agent_file_tools")
                && UPPER.contains(&name))
    })
}

fn token_paths(node: Node<'_>, source: &str) -> bool {
    let mut cursor = node.walk();
    let mut segment = None;
    let mut separator = false;
    for token in node.children(&mut cursor) {
        if matches!(token.kind(), "line_comment" | "block_comment") {
            continue;
        }
        let word = text(token, source);
        if matches!(token.kind(), "identifier" | "crate" | "super" | "self") {
            if separator && segment.is_some_and(|owner| forbidden(&[owner, word])) {
                return true;
            }
            segment = Some(word);
            separator = false;
        } else if word == "::" {
            separator = true;
        } else {
            segment = None;
            separator = false;
        }
    }
    false
}

// A grouped import extends its prefix for each branch independently. Aliases,
// wildcards and `self` cannot hide the module named on the left.
fn use_paths<'a>(node: Node<'_>, source: &'a str, path: &mut Vec<&'a str>) -> bool {
    let len = path.len();
    let found = match node.kind() {
        "use_list" => children(node)
            .into_iter()
            .any(|child| use_paths(child, source, path)),
        "scoped_use_list" => {
            if let Some(prefix) = node.child_by_field_name("path") {
                path.extend(text(prefix, source).split("::").map(str::trim));
            }
            forbidden(path) || use_paths(node.child_by_field_name("list").unwrap(), source, path)
        }
        "use_as_clause" => use_paths(node.child_by_field_name("path").unwrap(), source, path),
        "use_wildcard" => children(node)
            .into_iter()
            .any(|child| use_paths(child, source, path)),
        _ => {
            path.extend(text(node, source).split("::").map(str::trim));
            forbidden(path)
        }
    };
    path.truncate(len);
    found
}

fn ignored_file(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name == "tests.rs" || name.ends_with("_tests.rs") || path.iter().any(|part| part == "tests")
}

struct Guard {
    root: PathBuf,
    parser: Parser,
    findings: Vec<String>,
    facade: usize,
}

impl Guard {
    fn new(root: &Path) -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        Self {
            root: root.to_owned(),
            parser,
            findings: Vec::new(),
            facade: 0,
        }
    }

    fn file(&mut self, path: &Path, module_dir: &Path, root: bool) {
        // Match the library package's exclusions without treating inline test
        // modules as production merely because their parent file is shipped.
        if ignored_file(path.strip_prefix(&self.root).unwrap_or(path)) {
            return;
        }
        let source =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        self.source(path, module_dir, root, &source);
    }

    fn source(&mut self, path: &Path, module_dir: &Path, root: bool, source: &str) {
        let syntax = self.parser.parse(source, None).unwrap();
        self.walk(syntax.root_node(), path, module_dir, false, root, source);
    }

    fn walk(
        &mut self,
        node: Node<'_>,
        path: &Path,
        module_dir: &Path,
        inline: bool,
        root: bool,
        source: &str,
    ) {
        let kids = children(node);
        if kids.iter().any(|child| {
            child.kind() == "inner_attribute_item"
                && attribute(*child).is_some_and(|attr| test_cfg(attr, source))
        }) {
            return;
        }
        let mut attrs = Vec::new();
        for child in kids {
            match child.kind() {
                "attribute_item" => {
                    attrs.push(child);
                    continue;
                }
                "line_comment" | "block_comment" | "inner_attribute_item" => continue,
                _ => {}
            }
            if !is_test_item(&attrs, child, source) {
                if child.kind() == "mod_item" {
                    self.module(child, path, module_dir, inline, root, source, &attrs);
                } else {
                    self.check(child, path, source);
                    self.walk(child, path, module_dir, inline, false, source);
                }
            }
            attrs.clear();
        }
    }

    fn module(
        &mut self,
        node: Node<'_>,
        path: &Path,
        module_dir: &Path,
        inline: bool,
        root: bool,
        source: &str,
        attrs: &[Node<'_>],
    ) {
        let name = text(node.child_by_field_name("name").unwrap(), source);
        if root && UPPER.contains(&name) {
            return;
        }
        let override_path = attrs
            .iter()
            .filter_map(|it| attribute(*it))
            .find_map(|attr| {
                let key = attr.named_child(0)?;
                if text(key, source) != "path" {
                    return None;
                }
                Some(text(attr.child_by_field_name("value").unwrap(), source).trim_matches('"'))
            });
        let default_dir = module_dir.join(name);
        let override_base = if inline {
            module_dir
        } else {
            path.parent().unwrap()
        };
        if let Some(body) = node.child_by_field_name("body") {
            let nested = override_path.map_or(default_dir, |name| override_base.join(name));
            self.walk(body, path, &nested, true, false, source);
        } else if let Some(value) = override_path {
            let file = override_base.join(value);
            self.file(&file, file.parent().unwrap(), false);
        } else {
            let flat = module_dir.join(format!("{name}.rs"));
            let file = if flat.is_file() {
                flat
            } else {
                default_dir.join("mod.rs")
            };
            self.file(&file, &default_dir, false);
        }
    }

    fn check(&mut self, node: Node<'_>, path: &Path, source: &str) {
        let spelling = text(node, source);
        let found = match node.kind() {
            "use_declaration" => {
                if path == self.root.join("data_layer/trace_measure.rs") && spelling == TRACE_FACADE
                {
                    self.facade += 1;
                    if self.facade == 1 {
                        return;
                    }
                }
                use_paths(
                    node.child_by_field_name("argument").unwrap(),
                    source,
                    &mut Vec::new(),
                )
            }
            "scoped_identifier" | "scoped_type_identifier" | "scoped_use_list" => {
                // The containing import was already checked with its full prefix.
                if node.parent().is_some_and(|parent| {
                    matches!(
                        parent.kind(),
                        "scoped_identifier"
                            | "scoped_type_identifier"
                            | "scoped_use_list"
                            | "use_list"
                            | "use_as_clause"
                            | "use_declaration"
                    )
                }) {
                    return;
                }
                use_paths(node, source, &mut Vec::new())
            }
            "token_tree" => token_paths(node, source),
            _ => false,
        };
        if found {
            let line = node.start_position().row + 1;
            self.findings.push(format!(
                "{}:{line}: {spelling}",
                path.strip_prefix(&self.root).unwrap_or(path).display()
            ));
        }
    }
}

#[test]
fn production_engine_sources_do_not_reach_the_upper_crate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut guard = Guard::new(&root);
    guard.file(&root.join("lib.rs"), &root, true);
    assert!(guard.findings.is_empty(), "{}", guard.findings.join("\n"));
    assert_eq!(
        guard.facade, 1,
        "the only exception is the public trace facade"
    );
}

#[test]
fn production_engine_source_guard_distinguishes_test_code_and_path_segments() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let source = r#"
        #[cfg(all(test, target_os = "linux"))]
        mod test_only;
        mod sibling;
        fn adjacent() {
            #[cfg(test)]
            let _ = crate::commands::register();
            crate::commands::real();
        }
        #[test]
        fn standalone() { crate::commands::register(); }
        #[cfg(any(test, unix))]
        fn mixed() { super::super::commands::mixed(); }
        fn relative() { trace_measure::run(); }
        fn allowed() { crate::subc_fake::call(); trace_measure::build(); }
    "#;
    std::fs::write(root.join("test_only.rs"), "use crate::commands::test_only;").unwrap();
    std::fs::write(root.join("sibling.rs"), "use crate::{commands::sibling};").unwrap();
    let mut guard = Guard::new(root);
    guard.source(&root.join("lib.rs"), root, true, source);
    assert_eq!(guard.findings.len(), 4, "{:?}", guard.findings);
    for (finding, expected) in guard.findings.iter().zip([
        "commands::sibling",
        "commands::real",
        "commands::mixed",
        "trace_measure::run",
    ]) {
        assert!(finding.contains(expected), "{finding}");
    }

    let mut guard = Guard::new(root);
    guard.source(
        &root.join("data_layer/trace_measure.rs"),
        root,
        false,
        "pub use crate::trace_measure_run::run;\nuse crate::{trace_measure_run::{run as again}};",
    );
    assert_eq!(guard.facade, 1);
    assert_eq!(guard.findings.len(), 1, "{:?}", guard.findings);
    assert!(guard.findings[0].contains("run as again"));
}
