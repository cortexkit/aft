use std::collections::HashSet;
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
    "access_policy_tests",
    "nested_worktree_tests",
    "daemon_heap_probe",
    "search_hot_path_measurements",
    "engine_source_guard_tests",
    "upper_test_context",
    "test_context",
];
const REGISTERING_HELPER: &str = "aft-tools/src/upper_test_context.rs";

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
                    || (node.kind() == "function_item"
                        && attr.named_child(0).is_some_and(|name| {
                            matches!(text(name, source), "test" | "tokio::test")
                        }))
            })
    })
}

fn forbidden(path: &[&str]) -> bool {
    fn segment(part: &str) -> &str {
        part.strip_prefix("r#").unwrap_or(part)
    }
    path.windows(3).any(|parts| {
        parts
            .iter()
            .copied()
            .map(segment)
            .eq(["bash_rewrite", "dispatch", "register"])
    }) || path.windows(2).any(|pair| {
        let (owner, name) = (segment(pair[0]), segment(pair[1]));
        (owner == "trace_measure" && name == "run")
            || (matches!(owner, "crate" | "super" | "aft" | "agent_file_tools")
                && UPPER.contains(&name))
    })
}

fn token_paths(node: Node<'_>, source: &str) -> bool {
    let mut cursor = node.walk();
    let mut path = Vec::new();
    let mut separator = false;
    for token in node.children(&mut cursor) {
        if matches!(token.kind(), "line_comment" | "block_comment") {
            continue;
        }
        let word = text(token, source);
        if matches!(token.kind(), "identifier" | "crate" | "super" | "self") {
            if !separator {
                path.clear();
            }
            path.push(word);
            if forbidden(&path) {
                return true;
            }
            separator = false;
        } else if word == "::" {
            separator = true;
        } else {
            path.clear();
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

fn included_rust(node: Node<'_>, source: &str) -> Option<(String, bool)> {
    if node.kind() != "macro_invocation" {
        return None;
    }
    let expands = match text(node.child_by_field_name("macro")?, source) {
        "include" => true,
        "include_str" | "include_bytes" => false,
        _ => return None,
    };
    let args = children(node)
        .into_iter()
        .find(|child| child.kind() == "token_tree")?;
    let arg = args.named_child(0)?;
    if arg.kind() != "string_literal" {
        return None;
    }
    serde_json::from_str::<String>(text(arg, source))
        .ok()
        .filter(|path| path.ends_with(".rs"))
        .map(|path| (path, expands))
}

fn context_aliases<'a>(node: Node<'_>, source: &'a str, aliases: &mut Vec<&'a str>) {
    match node.kind() {
        "use_declaration" | "use_list" | "scoped_use_list" => {
            for child in children(node) {
                context_aliases(child, source, aliases);
            }
        }
        "use_as_clause" => {
            let path = text(node.child_by_field_name("path").unwrap(), source);
            if path.rsplit("::").next().unwrap().trim() == "AppContext" {
                let alias = text(node.child_by_field_name("alias").unwrap(), source);
                aliases.push(alias);
            }
        }
        _ => {}
    }
}

fn constructs_context(node: Node<'_>, source: &str, aliases: &[&str]) -> bool {
    if node.kind() == "call_expression" {
        let Some(function) = node.child_by_field_name("function") else {
            return false;
        };
        let function = if function.kind() == "generic_function" {
            function.child_by_field_name("function").unwrap_or(function)
        } else {
            function
        };
        let mut path = text(function, source)
            .rsplit("::")
            .map(|word| word.trim().trim_start_matches("r#"));
        return path.next() == Some("new")
            && path
                .next()
                .is_some_and(|owner| owner == "AppContext" || aliases.contains(&owner));
    }
    if node.kind() == "token_tree" {
        let mut cursor = node.walk();
        let tokens: Vec<_> = node
            .children(&mut cursor)
            .filter(|token| !matches!(token.kind(), "line_comment" | "block_comment"))
            .collect();
        return tokens.windows(4).any(|group| {
            let owner = text(group[0], source).trim_start_matches("r#");
            (owner == "AppContext" || aliases.contains(&owner))
                && group[1].kind() == "::"
                && text(group[2], source).trim_start_matches("r#") == "new"
                && group[3].kind() == "token_tree"
                && text(group[3], source).starts_with('(')
        });
    }
    false
}

#[derive(Clone, Copy)]
enum Scan {
    Engine,
    Area,
    UpperRegistration,
}

#[derive(Clone, Copy, Default)]
struct Scope {
    inline: bool,
    test: bool,
}

struct Guard {
    root: PathBuf,
    parser: Parser,
    findings: Vec<String>,
    scan: Scan,
    helper_calls: usize,
    upper_files: HashSet<PathBuf>,
}

impl Guard {
    fn new(root: &Path, scan: Scan) -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        Self {
            root: root.to_owned(),
            parser,
            findings: Vec::new(),
            scan,
            helper_calls: 0,
            upper_files: HashSet::new(),
        }
    }

    fn file(&mut self, path: &Path, module_dir: &Path, scope: Scope, aliases: &[&str]) {
        let source =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        if matches!(self.scan, Scan::UpperRegistration) {
            self.upper_files.insert(path.canonicalize().unwrap());
        }
        self.source(path, module_dir, scope, &source, aliases);
    }

    fn source(
        &mut self,
        path: &Path,
        module_dir: &Path,
        mut scope: Scope,
        source: &str,
        aliases: &[&str],
    ) {
        scope.test |= ignored_file(path.strip_prefix(&self.root).unwrap_or(path));
        // The registering helper also builds as a test-support library input.
        scope.test |= matches!(self.scan, Scan::UpperRegistration)
            && path == self.root.join(REGISTERING_HELPER);
        scope.inline = false;
        let syntax = self.parser.parse(source, None).unwrap();
        self.walk(syntax.root_node(), path, module_dir, scope, source, aliases);
    }

    fn walk(
        &mut self,
        node: Node<'_>,
        path: &Path,
        module_dir: &Path,
        mut scope: Scope,
        source: &str,
        outer_aliases: &[&str],
    ) {
        let kids = children(node);
        scope.test |= kids.iter().any(|child| {
            child.kind() == "inner_attribute_item"
                && attribute(*child).is_some_and(|attr| test_cfg(attr, source))
        });
        let mut aliases = outer_aliases.to_vec();
        if matches!(self.scan, Scan::UpperRegistration) {
            for child in &kids {
                context_aliases(*child, source, &mut aliases);
            }
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
            let mut child_scope = scope;
            child_scope.test |= is_test_item(&attrs, child, source);
            if child.kind() == "mod_item" {
                self.module(
                    child,
                    path,
                    module_dir,
                    child_scope,
                    source,
                    &attrs,
                    &aliases,
                );
            } else {
                if let Some((included, expands)) = included_rust(child, source) {
                    let file = path.parent().unwrap().join(included);
                    let upper = matches!(self.scan, Scan::Engine | Scan::Area)
                        && file
                            .canonicalize()
                            .is_ok_and(|file| self.upper_files.contains(&file));
                    if upper {
                        self.report(child, path, source);
                    } else if expands {
                        self.file(&file, module_dir, child_scope, &aliases);
                    }
                }
                match self.scan {
                    Scan::Engine | Scan::Area => self.check(child, path, source),
                    Scan::UpperRegistration if child_scope.test => {
                        self.check_registration(child, path, source, &aliases);
                    }
                    Scan::UpperRegistration => {}
                }
                self.walk(child, path, module_dir, child_scope, source, &aliases);
            }
            attrs.clear();
        }
    }

    fn module(
        &mut self,
        node: Node<'_>,
        path: &Path,
        module_dir: &Path,
        mut scope: Scope,
        source: &str,
        attrs: &[Node<'_>],
        aliases: &[&str],
    ) {
        let name = text(node.child_by_field_name("name").unwrap(), source);
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
        let override_base = if scope.inline {
            module_dir
        } else {
            path.parent().unwrap()
        };
        if let Some(body) = node.child_by_field_name("body") {
            let nested = override_path.map_or(default_dir, |name| override_base.join(name));
            scope.inline = true;
            self.walk(body, path, &nested, scope, source, aliases);
        } else if let Some(value) = override_path {
            let file = override_base.join(value);
            self.file(&file, file.parent().unwrap(), scope, aliases);
        } else {
            let flat = module_dir.join(format!("{name}.rs"));
            let file = if flat.is_file() {
                flat
            } else {
                default_dir.join("mod.rs")
            };
            self.file(&file, &default_dir, scope, aliases);
        }
    }

    fn check_registration(&mut self, node: Node<'_>, path: &Path, source: &str, aliases: &[&str]) {
        if !constructs_context(node, source, aliases) {
            return;
        }
        if path == self.root.join(REGISTERING_HELPER) {
            self.helper_calls += 1;
            return;
        }
        self.report(node, path, source);
    }

    fn check(&mut self, node: Node<'_>, path: &Path, source: &str) {
        if matches!(self.scan, Scan::Area)
            && matches!(
                node.kind(),
                "identifier" | "type_identifier" | "field_identifier"
            )
        {
            let name = text(node, source);
            if matches!(
                name.strip_prefix("r#").unwrap_or(name),
                "aft_engine" | "AppContext"
            ) {
                self.report(node, path, source);
                return;
            }
        }
        let found = match node.kind() {
            "use_declaration" => use_paths(
                node.child_by_field_name("argument").unwrap(),
                source,
                &mut Vec::new(),
            ),
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
            self.report(node, path, source);
        }
    }

    fn report(&mut self, node: Node<'_>, path: &Path, source: &str) {
        let line = node.start_position().row + 1;
        let spelling = text(node, source);
        self.findings.push(format!(
            "{}:{line}: {spelling}",
            path.strip_prefix(&self.root).unwrap_or(path).display()
        ));
    }
}

#[test]
fn production_engine_sources_do_not_reach_the_upper_crate() {
    let manifest = aft_base::test_support::manifest_dir();
    let root = manifest.join("src");
    let crates = manifest.join("..");
    let scope = Scope::default();
    let mut registration = Guard::new(&crates, Scan::UpperRegistration);
    for name in ["aft", "aft-tools", "aft-subc"] {
        let upper = crates.join(name).join("src");
        registration.file(&upper.join("lib.rs"), &upper, scope, &[]);
    }
    assert!(
        registration.findings.is_empty(),
        "use crate::test_context in upper library tests:\n{}",
        registration.findings.join("\n")
    );
    assert_eq!(
        registration.helper_calls, 1,
        "the helper builds one context"
    );

    for (source, scan) in [
        (root, Scan::Engine),
        (crates.join("aft-edit/src"), Scan::Area),
        (crates.join("aft-index/src"), Scan::Area),
        (crates.join("aft-inspect/src"), Scan::Area),
    ] {
        let mut guard = Guard::new(&source, scan);
        guard.upper_files.clone_from(&registration.upper_files);
        guard.file(&source.join("lib.rs"), &source, scope, &[]);
        assert!(guard.findings.is_empty(), "{}", guard.findings.join("\n"));
    }
}

#[test]
fn area_source_guard_rejects_engine_paths_and_context_in_tests() {
    let root = Path::new("src");
    let mut guard = Guard::new(root, Scan::Area);
    guard.source(
        &root.join("lib.rs"),
        root,
        Scope::default(),
        r#"
            #[cfg(test)]
            fn crossing() {
                use aft_engine::backup::BackupStore;
                let _: r#AppContext;
                let _ = crate::commands::run;
                let _ = "aft_engine::AppContext";
            }
        "#,
        &[],
    );
    assert_eq!(guard.findings.len(), 3, "{:?}", guard.findings);
    for (finding, expected) in
        guard
            .findings
            .iter()
            .zip(["aft_engine", "AppContext", "commands::run"])
    {
        assert!(finding.contains(expected), "{finding}");
    }
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
        fn standalone() { crate::bash_rewrite::dispatch::register(); }
        #[cfg(any(test, unix))]
        fn mixed() { super::super::commands::mixed(); }
        fn relative() { trace_measure::run(); }
        fn allowed() { crate::subc_fake::call(); trace_measure::build(); AppContext::new(); }
        const UPPER_INPUT: &str = include_str!("commands.rs");
    "#;
    std::fs::write(root.join("test_only.rs"), "use crate::commands::test_only;").unwrap();
    std::fs::write(root.join("sibling.rs"), "use crate::{commands::sibling};").unwrap();
    std::fs::write(root.join("commands.rs"), "").unwrap();
    let scope = Scope::default();
    let mut guard = Guard::new(root, Scan::Engine);
    guard
        .upper_files
        .insert(root.join("commands.rs").canonicalize().unwrap());
    guard.source(&root.join("lib.rs"), root, scope, source, &[]);
    assert_eq!(guard.findings.len(), 8, "{:?}", guard.findings);
    for (finding, expected) in guard.findings.iter().zip([
        "commands::test_only",
        "commands::sibling",
        "commands::register",
        "commands::real",
        "dispatch::register",
        "commands::mixed",
        "trace_measure::run",
        "include_str!",
    ]) {
        assert!(finding.contains(expected), "{finding}");
    }

    let mut guard = Guard::new(root, Scan::Engine);
    guard.source(
        &root.join("data_layer/trace_measure.rs"),
        root,
        Scope::default(),
        "pub use crate::trace_measure_run::run;\nuse crate::{trace_measure_run::{run as again}};",
        &[],
    );
    assert_eq!(guard.findings.len(), 2, "{:?}", guard.findings);
    assert!(guard.findings[0].contains("trace_measure_run::run"));
    assert!(guard.findings[1].contains("run as again"));

    let mut registration = Guard::new(root, Scan::UpperRegistration);
    registration.source(
        &root.join("lib.rs"),
        root,
        scope,
        r#"
        mod commands {
            fn production() { AppContext::new(); }
            use crate::context::AppContext as Outer;
            #[cfg(test)]
            mod tests {
                use super::*;
                use crate::{context::AppContext as Alias};
                fn direct() { crate::context::AppContext::new(); }
                fn alias() { Alias::new(); }
                fn inherited_alias() { Outer::new(); }
                fn helper() { crate::test_context(); }
            }
            #[test]
            fn standalone() { aft::context::AppContext::new(); }
        }
        #[cfg(test)]
        mod any_upper_module { fn direct() { AppContext::new(); } }
        "#,
        &[],
    );
    assert_eq!(
        registration.findings.len(),
        5,
        "{:?}",
        registration.findings
    );
    for (finding, expected) in registration.findings.iter().zip([
        "crate::context::AppContext::new()",
        "Alias::new()",
        "Outer::new()",
        "aft::context::AppContext::new()",
        "AppContext::new()",
    ]) {
        assert!(finding.contains(expected), "{finding}");
    }
}
