use std::fs;
use std::path::{Path, PathBuf};

use tree_sitter::{Node, Parser};

const NONCE_NAMES: &[&str] = &[
    "SUBC_LAUNCH_NONCE",
    "SUBC_LAUNCH_NONCE_FD",
    "SUBC_LAUNCH_NONCE_ENV",
    "SUBC_LAUNCH_NONCE_ENV_FD",
];
const NONCE_OWNER: &str = "crates/aft-base/src/launch_nonce.rs";
const CHILD_ENV: &str = "crates/aft-shell/src/agent_child_env.rs";

fn test_only(node: Node<'_>, source: &str) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(attribute) = previous {
        if is_comment(attribute) {
            previous = attribute.prev_named_sibling();
            continue;
        }
        if attribute.kind() != "attribute_item" {
            break;
        }
        let attr = compact(attribute, source);
        if attr == "#[cfg(test)]"
            || attr.starts_with("#[cfg(all(test,")
            || attr == "#[cfg(any(test,feature=\"test-support\"))]"
        {
            return true;
        }
        previous = attribute.prev_named_sibling();
    }
    false
}

fn consumer_identity_fields(node: Node<'_>, source: &str, lines: &mut Vec<usize>) {
    if is_comment(node) || test_only(node, source) {
        return;
    }
    // Fake-principal injection must stay test-only, including references that
    // do not assign a consumer_identity field at the call site.
    if node.kind() == "identifier" && text(node, source) == "with_test_vault_principal" {
        lines.push(node.start_position().row + 1);
    }
    let field = match node.kind() {
        "field_initializer" | "field_declaration" => node
            .child_by_field_name("field")
            .or_else(|| node.child_by_field_name("name")),
        "shorthand_field_initializer" => Some(node),
        "assignment_expression" => node.child_by_field_name("left").and_then(|left| {
            (left.kind() == "field_expression")
                .then(|| left.child_by_field_name("field"))
                .flatten()
        }),
        _ => None,
    };
    if field.is_some_and(|field| text(field, source) == "consumer_identity") {
        lines.push(node.start_position().row + 1);
    }
    // Macro bodies contain tokens rather than field-initializer nodes.
    if node.kind() == "token_tree" {
        let mut cursor = node.walk();
        for token in node.children(&mut cursor) {
            if text(token, source) == "consumer_identity"
                && token
                    .next_sibling()
                    .is_some_and(|next| text(next, source) == ":")
            {
                lines.push(token.start_position().row + 1);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        consumer_identity_fields(child, source, lines);
    }
}

fn rust_tree(source: &str) -> tree_sitter::Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    parser.parse(source, None).unwrap()
}

fn test_module_files(node: Node<'_>, source: &str, directory: &Path, files: &mut Vec<PathBuf>) {
    if node.kind() == "mod_item" {
        let name = text(node.child_by_field_name("name").unwrap(), source);
        if test_only(node, source) && node.child_by_field_name("body").is_none() {
            let mut attribute = node.prev_named_sibling();
            while let Some(attr) = attribute {
                if !matches!(
                    attr.kind(),
                    "attribute_item" | "line_comment" | "block_comment"
                ) {
                    break;
                }
                let value = compact(attr, source);
                if let Some(path) = value
                    .strip_prefix("#[path=\"")
                    .and_then(|s| s.strip_suffix("\"]"))
                {
                    files.push(directory.join(path));
                    return;
                }
                attribute = attr.prev_named_sibling();
            }
            files.push(directory.join(format!("{name}.rs")));
            files.push(directory.join(name).join("mod.rs"));
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        test_module_files(child, source, directory, files);
    }
}

#[test]
fn consumer_identity_is_selected_by_connection_not_production_calls() {
    let root = crate::test_helpers::manifest_dir().join("../..");
    let files = aft_rust_sources(&root);
    let sources: Vec<_> = files
        .iter()
        .map(|path| {
            let source = fs::read_to_string(path).unwrap();
            let tree = rust_tree(&source);
            (path, source, tree)
        })
        .collect();
    // Out-of-line test modules lack their parent's cfg attribute in their own
    // file. Discover them from declarations, not filename allowlists.
    let mut test_files = Vec::new();
    for (path, source, tree) in &sources {
        test_module_files(
            tree.root_node(),
            source,
            path.parent().unwrap(),
            &mut test_files,
        );
    }
    let mut failures = Vec::new();
    for (path, source, tree) in &sources {
        if test_files.contains(path) {
            continue;
        }
        let mut lines = Vec::new();
        consumer_identity_fields(tree.root_node(), source, &mut lines);
        for line in lines {
            failures.push(format!(
                "{}:{line}: leave consumer identity to the connection",
                path.display()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn consumer_identity_guard_detects_fields_and_excludes_only_test_code() {
    for source in [
        "fn f() { CallOptions { consumer_identity: None, ..Default::default() }; }",
        "fn f() { CallOptions { consumer_identity, ..Default::default() }; }",
        "fn f() { options.consumer_identity = Some(principal); }",
        "fn f() { wrapper!(CallOptions { consumer_identity: Some(principal) }); }",
        "#[cfg(test)] mod tests {} fn f() { CallOptions { consumer_identity: None }; }",
        "fn f() { with_test_vault_principal(options, principal); }",
        "fn f() { let hook = with_test_vault_principal; }",
        "#[cfg(any(test, feature = \"production\"))] fn f() { options.consumer_identity = None; }",
    ] {
        let mut lines = Vec::new();
        consumer_identity_fields(rust_tree(source).root_node(), source, &mut lines);
        assert_eq!(lines.len(), 1, "{source}");
    }
    let source = r#"
        // consumer_identity: is forbidden in calls, not in comments or strings.
        const MESSAGE: &str = "consumer_identity: example";
        #[cfg(test)] fn with_test_principal() { options.consumer_identity = None; }
        #[cfg(all(test, unix))] mod tests { fn f() { CallOptions { consumer_identity: None }; } }
        #[cfg(test)] fn with_test_vault_principal() {}
        #[cfg(any(test, feature = "test-support"))] fn f() { options.consumer_identity = None; }
        fn f() { #[cfg(test)] let options = with_test_vault_principal(options, principal); }
    "#;
    let mut lines = Vec::new();
    consumer_identity_fields(rust_tree(source).root_node(), source, &mut lines);
    assert!(lines.is_empty(), "{lines:?}");
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn compact(node: Node<'_>, source: &str) -> String {
    if is_comment(node) {
        return String::new();
    }
    if node.child_count() == 0 {
        return text(node, source)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
    }
    let mut cursor = node.walk();
    let result = node
        .children(&mut cursor)
        .map(|child| compact(child, source))
        .collect::<Vec<_>>()
        .join("");
    result
}

fn is_comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "comment" | "line_comment" | "block_comment")
}

fn is_test_module(node: Node<'_>, source: &str) -> bool {
    if node.kind() != "mod_item" {
        return false;
    }
    let mut previous = node.prev_named_sibling();
    while let Some(attribute) = previous {
        if is_comment(attribute) {
            previous = attribute.prev_named_sibling();
            continue;
        }
        if attribute.kind() != "attribute_item" {
            break;
        }
        if compact(attribute, source) == "#[cfg(test)]" {
            return true;
        }
        previous = attribute.prev_named_sibling();
    }
    false
}

fn has_nonce(node: Node<'_>, source: &str) -> bool {
    if is_comment(node) {
        return false;
    }
    if matches!(
        node.kind(),
        "identifier"
            | "property_identifier"
            | "string_literal"
            | "raw_string_literal"
            | "string"
            | "string_content"
            | "string_fragment"
    ) {
        let value = text(node, source);
        if NONCE_NAMES.contains(&value.trim_matches(['\'', '"', '`'])) {
            return true;
        }
    }
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .any(|child| has_nonce(child, source));
    found
}

fn env_function(function: &str, names: &[&str]) -> bool {
    names.iter().any(|name| {
        function == *name
            || function == format!("env::{name}")
            || function == format!("std::env::{name}")
    })
}

fn contains_env_iteration(node: Node<'_>, source: &str) -> bool {
    if node.kind() == "call_expression"
        && node
            .child_by_field_name("function")
            .is_some_and(|function| env_function(&compact(function, source), &["vars", "vars_os"]))
    {
        return true;
    }
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .any(|child| contains_env_iteration(child, source));
    found
}

fn visit(node: Node<'_>, source: &str, rust: bool, violations: &mut Vec<(usize, &'static str)>) {
    if is_comment(node) || (rust && is_test_module(node, source)) {
        return;
    }
    // Rust macro arguments are token trees, not expressions in the outer AST.
    // Parse their contents as a fragment so logging/assertion macros cannot
    // hide an otherwise direct reader from this guard.
    if rust && node.kind() == "macro_invocation" {
        let mut cursor = node.walk();
        if let Some(tokens) = node
            .named_children(&mut cursor)
            .find(|n| n.kind() == "token_tree")
        {
            let raw = text(tokens, source);
            let fragment = format!("fn __scan() {{ {} }}", &raw[1..raw.len() - 1]);
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(&fragment, None).unwrap();
            let mut nested = Vec::new();
            visit(tree.root_node(), &fragment, true, &mut nested);
            violations.extend(
                nested
                    .into_iter()
                    .map(|(line, rule)| (line + tokens.start_position().row, rule)),
            );
        }
        return;
    }
    if node.kind() == "call_expression" {
        if let Some(function) = node.child_by_field_name("function") {
            let name = compact(function, source);
            let rule = if rust
                && matches!(
                    name.as_str(),
                    "subc_os::launch_nonce" | "subc_client_rs::launch_nonce"
                ) {
                Some("shared launch_nonce reader outside the cache owner")
            } else if rust
                && env_function(&name, &["var", "var_os"])
                && node
                    .child_by_field_name("arguments")
                    .is_some_and(|args| has_nonce(args, source))
            {
                Some("direct launch nonce environment read")
            } else if rust
                && function.kind() == "field_expression"
                && function.child_by_field_name("field").is_some_and(|field| {
                    matches!(
                        text(field, source),
                        "filter" | "find" | "filter_map" | "any"
                    )
                })
                && contains_env_iteration(function, source)
                && node
                    .child_by_field_name("arguments")
                    .is_some_and(|args| has_nonce(args, source))
            {
                Some("launch nonce environment iteration/filter read")
            } else {
                None
            };
            if let Some(rule) = rule {
                violations.push((node.start_position().row + 1, rule));
            }
        }
    }
    if !rust && matches!(node.kind(), "member_expression" | "subscript_expression") {
        let object = node.child_by_field_name("object");
        let key = node
            .child_by_field_name("property")
            .or_else(|| node.child_by_field_name("index"));
        if object.is_some_and(|object| {
            matches!(compact(object, source).as_str(), "process.env" | "Bun.env")
        }) && key.is_some_and(|key| has_nonce(key, source))
        {
            // Assigning or deleting a key does not read its value. Compound
            // assignments still read the old value and must remain forbidden.
            let write_only = node.parent().is_some_and(|parent| {
                (parent.kind() == "assignment_expression"
                    && parent.child_by_field_name("left") == Some(node))
                    || (parent.kind() == "unary_expression"
                        && text(parent, source).trim_start().starts_with("delete "))
            });
            if !write_only {
                violations.push((
                    node.start_position().row + 1,
                    "direct launch nonce JS environment read",
                ));
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child, source, rust, violations);
    }
}

fn scan(path: &Path, source: &str) -> Vec<(usize, &'static str)> {
    // Only this exact file owns startup capture and the cached identity. A
    // same-named file in another directory is not an authorized reader.
    if path == Path::new(NONCE_OWNER) {
        return Vec::new();
    }
    let rust = path.extension().is_some_and(|ext| ext == "rs");
    let language = if rust {
        tree_sitter_rust::LANGUAGE.into()
    } else if path.extension().is_some_and(|ext| ext == "js") {
        tree_sitter_javascript::LANGUAGE.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    };
    let mut parser = Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).expect("source must parse");
    let mut violations = Vec::new();
    visit(tree.root_node(), source, rust, &mut violations);
    violations
}

fn collect(dir: &Path, extensions: &[&str], files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if extensions.contains(&"rs")
                || !matches!(
                    path.file_name().and_then(|s| s.to_str()),
                    Some("__tests__" | "dist" | "node_modules")
                )
            {
                collect(&path, extensions, files);
            }
        } else if path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| extensions.contains(&ext))
        {
            files.push(path);
        }
    }
}

fn collect_required(dir: &Path, extensions: &[&str], files: &mut Vec<PathBuf>) {
    let before = files.len();
    collect(dir, extensions, files);
    assert!(
        files.len() > before,
        "no source files under {}",
        dir.display()
    );
}

fn aft_rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for name in [
        "aft",
        "aft-base",
        "aft-db",
        "aft-engine",
        "aft-parse",
        "aft-subc",
        "aft-tools",
    ] {
        collect_required(
            &root.join("crates").join(name).join("src"),
            &["rs"],
            &mut files,
        );
    }
    files
}

#[test]
fn launch_nonce_has_only_one_source_reader() {
    let root = crate::test_helpers::manifest_dir().join("../..");
    let mut files = aft_rust_sources(&root);
    assert!(
        files.contains(&root.join(NONCE_OWNER)),
        "missing launch nonce owner: {NONCE_OWNER}"
    );
    for package in fs::read_dir(root.join("packages")).unwrap() {
        let src = package.unwrap().path().join("src");
        if src.is_dir() {
            collect(&src, &["ts", "mts", "js"], &mut files);
        }
    }
    files.sort();
    let mut failures = Vec::new();
    for file in files {
        let relative = file.strip_prefix(&root).unwrap();
        let source = fs::read_to_string(&file).unwrap();
        for (line, rule) in scan(relative, &source) {
            failures.push(format!("{}:{line}: {rule}", relative.display()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn scanner_detects_read_forms() {
    let rust = Path::new("crates/aft/src/other.rs");
    for source in [
        "fn f() { std::env::var(\"SUBC_LAUNCH_NONCE\"); }",
        "fn f() { std::env:: /* name */ var(r#\"SUBC_LAUNCH_NONCE\"#); }",
        "fn f() { println!(\"{:?}\", std::env::var(\"SUBC_LAUNCH_NONCE\")); }",
        "fn f() { std::env::vars_os().filter(|(k, _)| k == SUBC_LAUNCH_NONCE_ENV); }",
        "fn f() { env::var_os(subc_protocol::SUBC_LAUNCH_NONCE_ENV); }",
        "fn f() { var_os(\"SUBC_LAUNCH_NONCE_FD\"); }",
        "fn f() { env::var(SUBC_LAUNCH_NONCE_ENV_FD); }",
        "fn f() { std::env::vars().filter(|(k, _)| k == \"SUBC_LAUNCH_NONCE\"); }",
        "fn f() { subc_os::launch_nonce(); }",
        "fn f() { subc_client_rs::launch_nonce (); }",
        "#[cfg(test)] mod tests {} fn f() { env::var(\"SUBC_LAUNCH_NONCE\"); }",
    ] {
        assert_eq!(scan(rust, source).len(), 1, "{source}");
    }
    for source in [
        "const n = process.env.SUBC_LAUNCH_NONCE;",
        "const n = process.env['SUBC_LAUNCH_NONCE_FD'];",
        "const n = Bun.env[SUBC_LAUNCH_NONCE_ENV];",
        "const n = Bun.env.SUBC_LAUNCH_NONCE_FD;",
        "process.env.SUBC_LAUNCH_NONCE += 'x';",
    ] {
        assert_eq!(
            scan(Path::new("packages/plugin/src/example.ts"), source).len(),
            1,
            "{source}"
        );
    }
}

#[test]
fn scanner_allows_only_non_read_uses_and_cache_owner() {
    let rust = Path::new(CHILD_ENV);
    // agent_child_env.rs and sandbox_spawn.rs deny lists and removal calls
    // are allowed by operation, never by a blanket file exemption.
    let removals = r#"
const DENIED: &[&str] = &["SUBC_LAUNCH_NONCE", "SUBC_LAUNCH_NONCE_FD"];
fn scrub() {
    command.env_remove(subc_protocol::SUBC_LAUNCH_NONCE_ENV);
    std::env::remove_var("SUBC_LAUNCH_NONCE_FD");
    let example = "std::env::var(\"SUBC_LAUNCH_NONCE\")";
    // std::env::var("SUBC_LAUNCH_NONCE");
    /* subc_os::launch_nonce(); */
}
#[cfg(test)]
mod arbitrary_name { fn fixture() { std::env::var("SUBC_LAUNCH_NONCE"); } }
"#;
    assert!(scan(rust, removals).is_empty());
    assert!(scan(Path::new("crates/aft-shell/src/sandbox_spawn.rs"), removals).is_empty());
    assert!(scan(
        Path::new(NONCE_OWNER),
        "fn f() { subc_os::launch_nonce(); }"
    )
    .is_empty());
    assert_eq!(
        scan(
            Path::new("crates/aft/src/bin/launch_nonce.rs"),
            "fn f() { subc_os::launch_nonce(); }"
        )
        .len(),
        1
    );
    let js = "// process.env.SUBC_LAUNCH_NONCE\nconst example = 'process.env.SUBC_LAUNCH_NONCE'; process.env.SUBC_LAUNCH_NONCE = 'x'; delete Bun.env.SUBC_LAUNCH_NONCE;";
    assert!(scan(Path::new("packages/plugin/src/example.js"), js).is_empty());
    let root = crate::test_helpers::manifest_dir().join("../..");
    let actual = fs::read_to_string(root.join(CHILD_ENV)).unwrap();
    assert!(
        scan(rust, &actual).is_empty(),
        "child credential removals must remain allowed"
    );
}
