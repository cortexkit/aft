use super::{
    import_byte_range, ImportBlock, ImportForm, ImportGroup, ImportKind, ImportRequest,
    ImportStatement, ImportSyntax,
};
use tree_sitter::{Node, Tree};

const SWIFT_FLAT_MARKER_PREFIX: &str = "__aft_swift_import:";
const SWIFT_IMPORT_KINDS: &[&str] = &[
    "struct",
    "class",
    "func",
    "enum",
    "protocol",
    "typealias",
    "let",
    "var",
];
const SWIFT_STDLIB_MODULES: &[&str] = &[
    "Swift",
    "Foundation",
    "Dispatch",
    "Darwin",
    "Glibc",
    "UIKit",
    "AppKit",
    "SwiftUI",
    "Combine",
    "CoreData",
    "CoreFoundation",
    "CoreGraphics",
    "ObjectiveC",
    "XCTest",
    "os",
];

pub(crate) fn classify_group_swift(module_path: &str) -> ImportGroup {
    let top_level = module_path.split('.').next().unwrap_or(module_path);
    if SWIFT_STDLIB_MODULES.contains(&top_level) {
        ImportGroup::Stdlib
    } else {
        ImportGroup::External
    }
}

pub(crate) fn parse_swift_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();
    collect_swift_imports(source, root, &mut imports);
    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

fn collect_swift_imports(source: &str, node: Node, imports: &mut Vec<ImportStatement>) {
    if node.kind() == "import_declaration" {
        if let Some(imp) = parse_swift_import_declaration(source, &node) {
            imports.push(imp);
        }
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_swift_imports(source, cursor.node(), imports);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn parse_swift_import_declaration(source: &str, node: &Node) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let byte_range = node.byte_range();
    let mut modifiers = Vec::new();
    let mut import_kind = None;
    let mut module_path = None;

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            match child.kind() {
                "modifiers" => modifiers = parse_swift_modifiers(source, &child),
                kind if SWIFT_IMPORT_KINDS.contains(&kind) => {
                    import_kind = Some(source[child.byte_range()].trim().to_string());
                }
                "identifier" => {
                    module_path = Some(source[child.byte_range()].trim().to_string());
                }
                _ => {}
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let module_path = module_path?;
    if module_path.is_empty() {
        return None;
    }

    let names = Vec::new();
    let default_import = encode_swift_flat_marker(&modifiers, import_kind.as_deref());
    let group = classify_group_swift(&module_path);

    Some(ImportStatement {
        module_path,
        names: names.clone(),
        default_import,
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
        form: ImportForm::Structured {
            named: names,
            namespace: None,
            alias: None,
            modifiers,
            import_kind,
        },
    })
}

fn parse_swift_modifiers(source: &str, node: &Node) -> Vec<String> {
    let mut modifiers = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == "attribute" {
                let modifier = source[child.byte_range()].trim().to_string();
                if !modifier.is_empty() {
                    modifiers.push(modifier);
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    modifiers
}

fn encode_swift_flat_marker(modifiers: &[String], import_kind: Option<&str>) -> Option<String> {
    let import_kind = import_kind.filter(|kind| !kind.is_empty());
    if modifiers.is_empty() && import_kind.is_none() {
        return None;
    }

    Some(format!(
        "{}{}|{}",
        SWIFT_FLAT_MARKER_PREFIX,
        modifiers.join(","),
        import_kind.unwrap_or("")
    ))
}

fn decode_swift_flat_marker(marker: &str) -> Option<(Vec<String>, Option<String>)> {
    let body = marker.strip_prefix(SWIFT_FLAT_MARKER_PREFIX)?;
    let (modifiers, import_kind) = body.split_once('|').unwrap_or((body, ""));
    let modifiers = modifiers
        .split(',')
        .filter(|modifier| !modifier.is_empty())
        .map(str::to_string)
        .collect();
    let import_kind = (!import_kind.is_empty()).then(|| import_kind.to_string());
    Some((modifiers, import_kind))
}

pub(crate) fn generate_swift_import_line(req: &ImportRequest) -> String {
    let decoded = req.default_import.and_then(decode_swift_flat_marker);

    let modifier_storage;
    let modifiers: &[String] = if req.modifiers.is_empty() {
        if let Some((decoded_modifiers, _)) = decoded.as_ref() {
            modifier_storage = decoded_modifiers.clone();
            &modifier_storage
        } else {
            &[]
        }
    } else {
        req.modifiers
    };

    let import_kind_storage;
    let import_kind = if req.import_kind.is_none() {
        if let Some((_, decoded_import_kind)) = decoded.as_ref() {
            import_kind_storage = decoded_import_kind.clone();
            import_kind_storage.as_deref()
        } else {
            None
        }
    } else {
        req.import_kind
    };

    let mut line = String::new();
    if !modifiers.is_empty() {
        line.push_str(&modifiers.join(" "));
        line.push(' ');
    }
    line.push_str("import ");
    if let Some(kind) = import_kind.filter(|kind| !kind.is_empty()) {
        line.push_str(kind);
        line.push(' ');
    }
    line.push_str(req.module_path);
    line
}

pub struct SwiftSyntax;

impl ImportSyntax for SwiftSyntax {
    fn parse(&self, source: &str, tree: &Tree) -> ImportBlock {
        parse_swift_imports(source, tree)
    }

    fn generate_line(&self, req: &ImportRequest) -> String {
        generate_swift_import_line(req)
    }

    fn classify_group(&self, module_path: &str) -> ImportGroup {
        classify_group_swift(module_path)
    }
}

pub static SWIFT_SYNTAX: SwiftSyntax = SwiftSyntax;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{generate_import, parse_imports};
    use crate::parser::{grammar_for, LangId};
    use std::collections::BTreeSet;
    use tree_sitter::Parser;

    fn parse_swift(source: &str) -> (Tree, ImportBlock) {
        let grammar = grammar_for(LangId::Swift).unwrap();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let block = parse_imports(source, &tree, LangId::Swift);
        (tree, block)
    }

    /// Grammar fixture: lock the tree-sitter-swift node kinds this parser uses.
    /// The current grammar emits an `import_declaration` with optional direct
    /// `modifiers`, a direct kind token (`struct`, `class`, `func`, ...), and a
    /// direct `identifier` whose children are `simple_identifier` plus `.` for
    /// dotted paths.
    #[test]
    fn swift_grammar_node_kinds_are_stable() {
        let src = "import Foundation\nimport UIKit.UIView\n@testable import MyApp\n@_exported import Foo\nimport struct Foo.Bar\nimport class Foo.Baz\nimport func Foo.qux\nimport enum Foo.E\nimport protocol Foo.P\nimport typealias Foo.T\nimport let Foo.c\nimport var Foo.v\n\nstruct Test {}\n";
        let (tree, _) = parse_swift(src);
        assert!(!tree.root_node().has_error());

        let mut kinds = BTreeSet::new();
        fn walk(node: Node, kinds: &mut BTreeSet<String>) {
            kinds.insert(node.kind().to_string());
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    walk(cursor.node(), kinds);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
        }
        walk(tree.root_node(), &mut kinds);

        for required in [
            "import_declaration",
            "modifiers",
            "attribute",
            "@",
            "user_type",
            "type_identifier",
            "import",
            "identifier",
            "simple_identifier",
            ".",
            "struct",
            "class",
            "func",
            "enum",
            "protocol",
            "typealias",
            "let",
            "var",
        ] {
            assert!(
                kinds.contains(required),
                "swift grammar missing node kind {required:?}; present: {kinds:?}"
            );
        }
    }

    #[test]
    fn parse_swift_all_supported_forms() {
        let (_, block) = parse_swift(
            "import Foundation\nimport UIKit.UIView\n@testable import MyApp\n@_exported import Foo\nimport struct Foo.Bar\nimport class Foo.Baz\nimport func Foo.qux\nimport enum Foo.E\nimport protocol Foo.P\nimport typealias Foo.T\nimport let Foo.c\nimport var Foo.v\n\nstruct Test {}\n",
        );
        assert_eq!(block.imports.len(), 12);

        assert_swift_import(&block.imports[0], "Foundation", &[], None);
        assert_swift_import(&block.imports[1], "UIKit.UIView", &[], None);
        assert_swift_import(&block.imports[2], "MyApp", &["@testable"], None);
        assert_swift_import(&block.imports[3], "Foo", &["@_exported"], None);
        assert_swift_import(&block.imports[4], "Foo.Bar", &[], Some("struct"));
        assert_swift_import(&block.imports[5], "Foo.Baz", &[], Some("class"));
        assert_swift_import(&block.imports[6], "Foo.qux", &[], Some("func"));
        assert_swift_import(&block.imports[7], "Foo.E", &[], Some("enum"));
        assert_swift_import(&block.imports[8], "Foo.P", &[], Some("protocol"));
        assert_swift_import(&block.imports[9], "Foo.T", &[], Some("typealias"));
        assert_swift_import(&block.imports[10], "Foo.c", &[], Some("let"));
        assert_swift_import(&block.imports[11], "Foo.v", &[], Some("var"));
    }

    fn assert_swift_import(
        imp: &ImportStatement,
        module_path: &str,
        expected_modifiers: &[&str],
        expected_import_kind: Option<&str>,
    ) {
        let expected_modifiers: Vec<String> = expected_modifiers
            .iter()
            .map(|modifier| modifier.to_string())
            .collect();

        assert_eq!(imp.module_path, module_path);
        assert_eq!(imp.names, Vec::<String>::new());
        assert_eq!(imp.namespace_import, None);
        assert_eq!(imp.kind, ImportKind::Value);
        assert_eq!(imp.group, classify_group_swift(module_path));
        assert_eq!(
            imp.default_import,
            encode_swift_flat_marker(&expected_modifiers, expected_import_kind)
        );

        assert_eq!(
            imp.form,
            ImportForm::Structured {
                named: vec![],
                namespace: None,
                alias: None,
                modifiers: expected_modifiers,
                import_kind: expected_import_kind.map(str::to_string),
            }
        );
    }

    #[test]
    fn generate_swift_supported_forms() {
        assert_eq!(
            generate_import(
                LangId::Swift,
                &ImportRequest::legacy("Foundation", &[], None, None, false)
            ),
            "import Foundation"
        );

        let testable = vec!["@testable".to_string()];
        assert_eq!(
            generate_import(
                LangId::Swift,
                &ImportRequest {
                    module_path: "MyApp",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &testable,
                    import_kind: None,
                }
            ),
            "@testable import MyApp"
        );

        assert_eq!(
            generate_import(
                LangId::Swift,
                &ImportRequest {
                    module_path: "Foo.Bar",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &[],
                    import_kind: Some("struct"),
                }
            ),
            "import struct Foo.Bar"
        );

        let exported = vec!["@_exported".to_string()];
        assert_eq!(
            generate_import(
                LangId::Swift,
                &ImportRequest {
                    module_path: "Foo",
                    names: &[],
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers: &exported,
                    import_kind: Some("class"),
                }
            ),
            "@_exported import class Foo"
        );
    }

    #[test]
    fn generate_swift_preserves_structured_forms_from_legacy_flat_marker() {
        for (modifiers, import_kind, module_path, expected) in [
            (
                vec!["@testable".to_string()],
                None,
                "MyApp",
                "@testable import MyApp",
            ),
            (vec![], Some("struct"), "Foo.Bar", "import struct Foo.Bar"),
            (
                vec!["@_exported".to_string()],
                Some("class"),
                "Foo",
                "@_exported import class Foo",
            ),
        ] {
            let marker = encode_swift_flat_marker(&modifiers, import_kind);
            assert_eq!(
                generate_import(
                    LangId::Swift,
                    &ImportRequest::legacy(module_path, &[], marker.as_deref(), None, false)
                ),
                expected
            );
        }
    }

    #[test]
    fn classify_group_swift_stdlib_vs_external() {
        assert_eq!(classify_group_swift("Swift"), ImportGroup::Stdlib);
        assert_eq!(classify_group_swift("Foundation"), ImportGroup::Stdlib);
        assert_eq!(classify_group_swift("UIKit.UIView"), ImportGroup::Stdlib);
        assert_eq!(classify_group_swift("MyApp"), ImportGroup::External);
        assert_eq!(classify_group_swift("Foo.Bar"), ImportGroup::External);
    }

    #[test]
    fn swift_round_trips_through_parse_generate() {
        for src in [
            "import Foundation",
            "import UIKit.UIView",
            "@testable import MyApp",
            "@_exported import Foo",
            "import struct Foo.Bar",
            "import class Foo.Baz",
            "import func Foo.qux",
            "import enum Foo.E",
            "import protocol Foo.P",
            "import typealias Foo.T",
            "import let Foo.c",
            "import var Foo.v",
            "@testable import struct Foo.Bar",
        ] {
            let (_, block) = parse_swift(src);
            assert_eq!(block.imports.len(), 1, "parse {src:?}");
            let imp = &block.imports[0];
            let (modifiers, import_kind) = match &imp.form {
                ImportForm::Structured {
                    modifiers,
                    import_kind,
                    ..
                } => (modifiers.as_slice(), import_kind.as_deref()),
                other => panic!("expected Swift Structured import, got {other:?}"),
            };
            let regenerated = generate_import(
                LangId::Swift,
                &ImportRequest {
                    module_path: &imp.module_path,
                    names: &imp.names,
                    default_import: None,
                    namespace: None,
                    alias: None,
                    type_only: false,
                    modifiers,
                    import_kind,
                },
            );
            assert_eq!(regenerated, src, "round-trip mismatch for {src:?}");
        }
    }
}
