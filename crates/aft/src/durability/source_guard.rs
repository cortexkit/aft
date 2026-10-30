use std::path::{Path, PathBuf};

// No production exceptions are needed. Any future exception must identify one
// exact source line and explain why the shared durability helpers cannot be used.
const ALLOWLIST: &[(&str, &str, &str)] = &[];

// These entire source files are test-only, rather than containing cfg(test) items.
// Check their owning declarations so moving them into production cannot hide calls.
const TEST_MODULES: &[(&str, &str, &str)] = &[
    (
        "disk_write_hunt.rs",
        include_str!("../lib.rs"),
        "#[cfg(all(test,target_os=\"macos\"))]moddisk_write_hunt;",
    ),
    (
        "build_breaker_audit_tests.rs",
        include_str!("../build_breaker.rs"),
        "#[cfg(test)]#[path=\"build_breaker_audit_tests.rs\"]modaudit_matrix_tests;",
    ),
];

fn requires_test(expression: &str) -> bool {
    if expression == "test" {
        return true;
    }
    let (all, arguments) = if let Some(arguments) = expression.strip_prefix("all(") {
        (true, arguments)
    } else if let Some(arguments) = expression.strip_prefix("any(") {
        (false, arguments)
    } else {
        return false;
    };
    let Some(arguments) = arguments.strip_suffix(')') else {
        return false;
    };
    let mut depth = 0;
    let mut start = 0;
    let mut values = Vec::new();
    for (index, byte) in arguments.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                values.push(requires_test(&arguments[start..index]));
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < arguments.len() {
        values.push(requires_test(&arguments[start..]));
    }
    if all {
        values.into_iter().any(|value| value)
    } else {
        !values.is_empty() && values.into_iter().all(|value| value)
    }
}

fn test_attribute(attribute: &str) -> bool {
    let attribute: String = attribute.chars().filter(|ch| !ch.is_whitespace()).collect();
    attribute == "#[test]"
        || attribute
            .strip_prefix("#[cfg(")
            .or_else(|| attribute.strip_prefix("#![cfg("))
            .and_then(|value| value.strip_suffix(")]"))
            .is_some_and(requires_test)
}

fn blank(range: std::ops::Range<usize>, bytes: &mut [u8]) {
    for byte in &mut bytes[range] {
        if !matches!(*byte, b'\n' | b'\r') {
            *byte = b' ';
        }
    }
}

fn mask(node: tree_sitter::Node<'_>, source: &str, bytes: &mut [u8]) {
    let kind = node.kind();
    if kind.ends_with("comment") || kind.contains("string_literal") || kind == "char_literal" {
        blank(node.byte_range(), bytes);
        return;
    }
    let mut cursor = node.walk();
    if node.named_children(&mut cursor).any(|child| {
        child.kind() == "inner_attribute_item" && test_attribute(&source[child.byte_range()])
    }) {
        blank(node.byte_range(), bytes);
        return;
    }
    let mut test_only = false;
    for child in node.named_children(&mut cursor) {
        if child.kind() == "attribute_item" {
            test_only |= test_attribute(&source[child.byte_range()]);
            blank(child.byte_range(), bytes);
        } else if child.kind().ends_with("comment") {
            mask(child, source, bytes);
        } else {
            if test_only {
                blank(child.byte_range(), bytes);
            } else {
                mask(child, source, bytes);
            }
            test_only = false;
        }
    }
}

fn direct_sync_lines(source: &str) -> Vec<usize> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut bytes = source.as_bytes().to_vec();
    mask(tree.root_node(), source, &mut bytes);
    let production = String::from_utf8(bytes).unwrap();
    let calls = regex::Regex::new(r"(\.|::)\s*(sync_all|sync_data)\s*\(").unwrap();
    calls
        .captures_iter(&production)
        .filter_map(|call| {
            let matched = call.get(0).unwrap();
            let prefix = production[..matched.start()].trim_end();
            let module = prefix
                .rsplit(|ch: char| !ch.is_alphanumeric() && ch != '_')
                .next();
            if &call[1] == "::" && &call[2] == "sync_data" && module == Some("durability") {
                return None;
            }
            Some(
                source[..matched.start()]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count()
                    + 1,
            )
        })
        .collect()
}

fn rust_sources(directory: &Path, paths: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            rust_sources(&entry.path(), paths);
        } else if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "rs")
        {
            paths.push(entry.path());
        }
    }
}

#[test]
fn production_syncs_use_shared_durability_helpers() {
    for (_, owner, declaration) in TEST_MODULES {
        let normalized: String = owner.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(
            normalized.contains(declaration),
            "test-only module declaration changed: {declaration}"
        );
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut paths = Vec::new();
    rust_sources(&root, &mut paths);
    paths.sort();
    assert!(paths.len() > 100, "source scan did not cover the crate");
    let mut violations = Vec::new();
    let mut used = vec![false; ALLOWLIST.len()];
    for path in paths {
        let relative = path
            .strip_prefix(&root)
            .unwrap()
            .to_str()
            .unwrap()
            .replace('\\', "/");
        if relative == "durability.rs" || TEST_MODULES.iter().any(|(file, _, _)| *file == relative)
        {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\r\n", "\n");
        for line in direct_sync_lines(&source) {
            let text = source.lines().nth(line - 1).unwrap().trim();
            if let Some(index) = ALLOWLIST.iter().position(|(file, code, reason)| {
                assert!(!reason.trim().is_empty(), "sync exception needs a reason");
                *file == relative && *code == text
            }) {
                used[index] = true;
            } else {
                violations.push(format!("{relative}:{line}: {text}"));
            }
        }
    }
    assert!(
        used.iter().all(|used| *used),
        "remove stale direct-sync exceptions"
    );
    assert!(
        violations.is_empty(),
        "production syncs must use crate::durability helpers:\n{}",
        violations.join("\n")
    );
}

#[test]
fn scan_rejects_method_ufcs_and_macro_sync_calls() {
    let source = r#"
fn normal(file: &File) { file.sync_all().unwrap(); }
fn data(file: &File) { file /* comment */ . sync_data ().unwrap(); }
fn ufcs(file: &File) { std::fs::File::sync_all(file).unwrap(); }
macro_rules! commit { ($file:expr) => { $file.sync_all().unwrap() }; }
#[cfg(not(test))]
fn production(file: &File) { File::sync_data(file).unwrap(); }
#[cfg(any(test, unix))]
fn also_production(file: &File) { file.sync_all().unwrap(); }
"#;
    assert_eq!(direct_sync_lines(source), vec![2, 3, 4, 5, 7, 9]);
}

#[test]
fn scan_allows_test_items_literals_comments_and_shared_helpers() {
    let source = r##"
// file.sync_all()
/* file.sync_data() */
const EXAMPLE: &str = "file.sync_all()";
const RAW: &str = r#"File::sync_data(file)"#;
fn shared(file: &File) { crate::durability::sync_data(file, Path::new("state")).unwrap(); }
#[cfg(test)]
mod tests { fn fixture(file: &File) { file.sync_all().unwrap(); } }
#[cfg(all(unix, test))]
fn fixture(file: &File) { file.sync_data().unwrap(); }
#[cfg(any(all(test, unix), all(test, windows)))]
fn other_fixture(file: &File) { file.sync_all().unwrap(); }
#[test]
fn test_case() { file.sync_all().unwrap(); }
"##;
    assert!(direct_sync_lines(source).is_empty());
    assert!(direct_sync_lines(
        "#![cfg(test)]\nfn fixture(file: &File) { file.sync_all().unwrap(); }"
    )
    .is_empty());
}
