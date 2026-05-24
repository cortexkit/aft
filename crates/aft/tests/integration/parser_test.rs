use aft::parser::FileParser;

#[test]
fn python_decorated_function_range_includes_decorators() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("decorated.py");
    std::fs::write(&file, "@cache\n@profile\ndef f():\n    pass\n").expect("write python file");

    let mut parser = FileParser::new();
    let symbols = parser.extract_symbols(&file).expect("extract symbols");
    let symbol = symbols
        .iter()
        .find(|sym| sym.name == "f")
        .expect("find decorated function");

    assert_eq!(symbol.range.start_line, 0, "range should start at @cache");
    assert_eq!(symbol.range.start_col, 0);
}

#[test]
fn symbol_cache_detects_same_mtime_content_edit() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("cached.rs");
    std::fs::write(&file, "pub fn alpha() {}\n").expect("write rust file");
    let original_mtime = std::fs::metadata(&file)
        .expect("stat rust file")
        .modified()
        .expect("mtime");

    let mut parser = FileParser::new();
    let first = parser
        .extract_symbols(&file)
        .expect("extract first symbols");
    assert!(first.iter().any(|symbol| symbol.name == "alpha"));

    std::fs::write(&file, "pub fn bravo() {}\n").expect("rewrite rust file same size");
    filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(original_mtime))
        .expect("restore original mtime");

    let second = parser
        .extract_symbols(&file)
        .expect("extract second symbols");
    assert!(second.iter().any(|symbol| symbol.name == "bravo"));
    assert!(!second.iter().any(|symbol| symbol.name == "alpha"));
}

#[test]
fn ts_export_clause_marks_local_symbol_exported() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("exports.ts");
    std::fs::write(&file, "function foo() { return 1; }\nexport { foo };\n")
        .expect("write ts file");

    let mut parser = FileParser::new();
    let symbols = parser.extract_symbols(&file).expect("extract symbols");
    let foo = symbols
        .iter()
        .find(|symbol| symbol.name == "foo")
        .expect("find foo");
    assert!(foo.exported, "foo should be exported via export clause");
}

#[test]
fn ts_export_default_identifier_marks_local_symbol_exported() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("default.ts");
    std::fs::write(&file, "function foo() { return 1; }\nexport default foo;\n")
        .expect("write ts file");

    let mut parser = FileParser::new();
    let symbols = parser.extract_symbols(&file).expect("extract symbols");
    let foo = symbols
        .iter()
        .find(|symbol| symbol.name == "foo")
        .expect("find foo");
    assert!(
        foo.exported,
        "foo should be exported via default identifier"
    );
}
