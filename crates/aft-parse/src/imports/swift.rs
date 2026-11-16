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

pub fn classify_group_swift(module_path: &str) -> ImportGroup {
    let top_level = module_path.split('.').next().unwrap_or(module_path);
    if SWIFT_STDLIB_MODULES.contains(&top_level) {
        ImportGroup::Stdlib
    } else {
        ImportGroup::External
    }
}

pub fn parse_swift_imports(source: &str, tree: &Tree) -> ImportBlock {
    let root = tree.root_node();
    let mut imports = Vec::new();
    collect_swift_imports(source, root, &mut imports);
    let byte_range = import_byte_range(&imports);
    ImportBlock {
        imports,
        byte_range,
    }
}

/// Collects the import declarations under `root` in source order. The walk
/// keeps its position in a tree cursor rather than on the call stack: a
/// file can nest tens of thousands of levels deep (the Swift compiler's own
/// tests hold one of ~50,000 nested parentheses), and a recursive walk
/// overflowed the thread's stack on it, which aborts the whole process.
fn collect_swift_imports(source: &str, root: Node, imports: &mut Vec<ImportStatement>) {
    super::for_each_preorder_until(root, "import_declaration", |node| {
        if let Some(imp) = parse_swift_import_declaration(source, &node) {
            imports.push(imp);
        }
    });
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

pub fn generate_swift_import_line(req: &ImportRequest) -> String {
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
