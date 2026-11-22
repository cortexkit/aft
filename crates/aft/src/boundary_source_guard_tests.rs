use std::path::{Path, PathBuf};

use tree_sitter::{Node, Parser};

// These root modules stay above the tools library when the crate is split.
const FACADE_MODULES: &[&str] = &["subc"];

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn segment(node: Node<'_>, source: &str) -> bool {
    matches!(node.kind(), "identifier" | "type_identifier")
        && matches!(text(node, source), "subc" | "r#subc")
}

// Walk only the imported paths, not alias names. With grouped imports each
// leaf is a path segment, even if there is no adjacent `::` in the source.
fn imports_subc(node: Node<'_>, source: &str) -> bool {
    match node.kind() {
        "use_as_clause" => imports_subc(node.child_by_field_name("path").unwrap(), source),
        "scoped_identifier" | "scoped_use_list" | "use_list" | "use_wildcard" => node
            .named_children(&mut node.walk())
            .any(|child| imports_subc(child, source)),
        _ => segment(node, source),
    }
}

fn token_path(node: Node<'_>, source: &str) -> bool {
    let mut cursor = node.walk();
    let tokens = node
        .children(&mut cursor)
        .filter(|token| !token.is_extra())
        .collect::<Vec<_>>();
    tokens.windows(2).any(|pair| {
        (segment(pair[0], source) && text(pair[1], source) == "::")
            || (text(pair[0], source) == "::" && segment(pair[1], source))
    })
}

fn source_path(value: Node<'_>, source: &str, file: &Path) -> String {
    let spelling = text(value, source);
    if value.kind() == "raw_string_literal" {
        let (_, quoted) = spelling.split_once('"').unwrap();
        quoted.rsplit_once('"').unwrap().0.to_owned()
    } else {
        serde_json::from_str(spelling)
            .unwrap_or_else(|_| panic!("unsupported module path in {}: {spelling}", file.display()))
    }
}

struct Guard {
    src: PathBuf,
    parser: Parser,
    findings: Vec<String>,
}

impl Guard {
    fn scan(src: &Path) -> Vec<String> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let mut guard = Self {
            src: src.canonicalize().unwrap(),
            parser,
            findings: Vec::new(),
        };
        let root = guard.src.clone();
        guard.file(&root.join("lib.rs"), &root);
        guard.findings
    }

    fn file(&mut self, file: &Path, module_dir: &Path) {
        let path = file
            .canonicalize()
            .unwrap_or_else(|error| panic!("cannot locate {}: {error}", file.display()));
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let tree = self.parser.parse(&source, None).unwrap();
        self.walk(tree.root_node(), &path, module_dir, &source, false);
    }

    fn walk(&mut self, node: Node<'_>, file: &Path, dir: &Path, source: &str, inline: bool) {
        let mut attrs = Vec::new();
        let mut cursor = node.walk();
        for child in node
            .named_children(&mut cursor)
            .filter(|child| !child.is_extra())
        {
            if child.kind() == "attribute_item" {
                attrs.extend(child.named_child(0));
                continue;
            }
            match child.kind() {
                "mod_item" => self.module(child, file, dir, source, inline, &attrs),
                "macro_invocation"
                    if child
                        .child_by_field_name("macro")
                        .is_some_and(|name| text(name, source) == "include") =>
                {
                    // Unlike include_str!, include! inserts Rust into this module.
                    let args = child.named_child(1).unwrap();
                    let literal = args.named_child(0).unwrap();
                    let path = file
                        .parent()
                        .unwrap()
                        .join(source_path(literal, source, file));
                    self.file(&path, dir);
                }
                "use_declaration" => {
                    if imports_subc(child.child_by_field_name("argument").unwrap(), source) {
                        self.report(child, file, source);
                    }
                }
                _ => {
                    let reference = match child.kind() {
                        "scoped_identifier" | "scoped_type_identifier" => child
                            .named_children(&mut child.walk())
                            .any(|part| segment(part, source)),
                        "token_tree" => token_path(child, source),
                        _ => false,
                    };
                    if reference {
                        self.report(child, file, source);
                    }
                    self.walk(child, file, dir, source, inline);
                }
            }
            attrs.clear();
        }
    }

    fn module(
        &mut self,
        node: Node<'_>,
        file: &Path,
        dir: &Path,
        source: &str,
        inline: bool,
        attrs: &[Node<'_>],
    ) {
        let name = text(node.child_by_field_name("name").unwrap(), source);
        let name = name.strip_prefix("r#").unwrap_or(name);
        let at_root = node
            .parent()
            .is_some_and(|parent| parent.kind() == "source_file")
            && file == self.src.join("lib.rs");
        if at_root && FACADE_MODULES.contains(&name) {
            return;
        }
        let override_path = attrs.iter().find_map(|attr| {
            if text(attr.named_child(0)?, source) != "path" {
                return None;
            }
            let name = source_path(attr.child_by_field_name("value").unwrap(), source, file);
            let base = if inline { dir } else { file.parent().unwrap() };
            Some(base.join(name))
        });
        let module_dir = dir.join(name);
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(
                body,
                file,
                override_path.as_deref().unwrap_or(&module_dir),
                source,
                true,
            );
        } else if let Some(path) = override_path {
            self.file(&path, path.parent().unwrap());
        } else {
            let flat = dir.join(format!("{name}.rs"));
            let path = if flat.is_file() {
                flat
            } else {
                module_dir.join("mod.rs")
            };
            self.file(&path, &module_dir);
        }
    }

    fn report(&mut self, node: Node<'_>, file: &Path, source: &str) {
        self.findings.push(format!(
            "{}:{}: {}",
            file.strip_prefix(&self.src).unwrap_or(file).display(),
            node.start_position().row + 1,
            text(node, source),
        ));
    }
}

#[test]
fn tools_module_tree_does_not_reference_subc() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let forbidden = Guard::scan(&src);
    assert!(
        forbidden.is_empty(),
        "tools reference the daemon module:\n{}",
        forbidden.join("\n")
    );
}

#[test]
fn source_guard_detects_planted_paths_in_tools_modules_only() {
    let checkout = tempfile::tempdir().unwrap();
    let src = checkout.path().join("src");
    let write = |path: &str, source| {
        let path = checkout.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    };
    write(
        "src/lib.rs",
        r##"
        pub mod subc;
        mod tools;
        mod inline {
            #[path = r#"overridden.rs"#]
            mod loaded;
        }
        #[cfg(all(unix, any(/* a comment */ test, all(test, windows))))]
        mod conditional;
        #[cfg(test)]
        #[path = "../tests/helper.rs"]
        mod fixture;
        mod tests { use crate::subc_fake; }
        mod layout_tests;
        mod inner_only { #![cfg(test)] use crate::subc_config; }
        #[test]
        fn fixture_test() { crate::subc_format::test(); }
        fn cfg_in_block() {
            #[cfg(test)]
            let _ = crate::subc_format::test();
            crate::subc_translate::real();
        }
        use crate::{subc_config, subc_translate, subc_fake, subc_format};
        use crate::other as subc;
        const TEXT: &str = "aft::subc";
        /// crate::subc is permitted in docs.
        fn subc() {}
    "##,
    );
    for file in [
        "src/subc/mod.rs",
        "src/main.rs",
        "src/bin/helper.rs",
        "tests/not_linked.rs",
        "src/not_a_module.rs",
    ] {
        write(file, "use crate::subc;");
    }
    write(
        "src/tools.rs",
        "mod nested; #[path = \"tests/helper.rs\"] #[cfg(test)] mod helper; include!(\"included.rs\");",
    );
    for file in [
        "tests/helper.rs",
        "src/conditional.rs",
        "src/layout_tests.rs",
        "src/tests/helper.rs",
        "src/included.rs",
    ] {
        write(file, "use crate::subc_fake;");
    }
    write("src/tools/nested/mod.rs", "mod leaf;");
    write("src/tools/nested/leaf.rs", "const OK: &str = \"subc\";");
    write("src/inline/overridden.rs", "mod child;");
    write("src/inline/child.rs", "use crate::subc_config;");
    assert_eq!(Guard::scan(&src), Vec::<String>::new());

    write(
        "src/tools/nested/leaf.rs",
        "use crate::{other, subc};\nuse super::{subc::{self as daemon, thing}};",
    );
    write(
        "src/inline/child.rs",
        r#"
        #[cfg(any(test, unix))]
        fn planted() { crate /* spaced */ :: r#subc :: call(); }
        type Planted = aft::subc::Type;
        fn macro_path() { call!(super::subc::run()); }
    "#,
    );
    let findings = Guard::scan(&src);
    assert_eq!(findings.len(), 5, "{findings:#?}");
    for (finding, (path, spelling)) in findings.iter().zip([
        ("tools/nested/leaf.rs", "crate::{other, subc}"),
        (
            "tools/nested/leaf.rs",
            "super::{subc::{self as daemon, thing}}",
        ),
        ("inline/child.rs", "r#subc"),
        ("inline/child.rs", "aft::subc"),
        ("inline/child.rs", "super::subc"),
    ]) {
        let path: PathBuf = path.split('/').collect();
        assert!(
            finding.starts_with(path.to_str().unwrap()) && finding.contains(spelling),
            "{finding}"
        );
    }

    for (file, marker) in [
        ("tests/helper.rs", "fixture"),
        ("src/conditional.rs", "conditional"),
        ("src/layout_tests.rs", "layout"),
        ("src/tests/helper.rs", "linked_tests_dir"),
        ("src/included.rs", "included"),
    ] {
        write(file, &format!("use crate::{{subc as {marker}}};"));
    }
    write(
        "src/cfg_shapes.rs",
        "mod tests { use super::subc; }\n\
         mod inner_test { #![cfg(test)] use crate::subc; }\n\
         #[test] fn planted() { aft::subc::run(); }",
    );
    let root = src.join("lib.rs");
    let source = std::fs::read_to_string(&root).unwrap();
    write(
        "src/lib.rs",
        &format!("{source}\n#[cfg(test)] mod cfg_shapes;"),
    );
    let found = Guard::scan(&src);
    assert_eq!(found.len(), findings.len() + 8, "{found:#?}");
    for marker in [
        "fixture",
        "conditional",
        "layout",
        "linked_tests_dir",
        "included",
    ] {
        assert!(
            found
                .iter()
                .any(|line| line.ends_with(&format!("subc as {marker}}};"))),
            "{found:#?}"
        );
    }
    assert_eq!(
        found
            .iter()
            .filter(|line| line.starts_with("cfg_shapes.rs:"))
            .count(),
        3
    );
}
