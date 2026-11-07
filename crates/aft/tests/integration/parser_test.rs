use aft::parser::FileParser;

#[test]
fn swift_try_cast_coalescing_expressions_have_no_parse_errors() {
    let tmp = crate::test_helpers::tempdir();
    let file = tmp.path().join("CastExpressions.swift");
    let mut failures = Vec::new();
    let mut checked = 0;
    for prefix in ["", "try ", "try? ", "try! "] {
        let call = if prefix.is_empty() {
            "value()"
        } else {
            "throwingValue()"
        };
        for cast in ["as?", "as!"] {
            for (ty, default) in [("[String: Any]", "[:]"), ("[Any]", "[]")] {
                for coalescing in [false, true] {
                    let fallback = if coalescing {
                        format!(" ?? {default}")
                    } else {
                        String::new()
                    };
                    let expression = format!("{prefix}{call} {cast} {ty}{fallback}");
                    for (context, function) in [
                        (
                            "let",
                            format!("func probe() throws {{ let json = {expression}; _ = json }}"),
                        ),
                        (
                            "return",
                            format!("func probe() throws -> Any? {{ return {expression} }}"),
                        ),
                        (
                            "argument",
                            format!("func probe() throws {{ consume({expression}) }}"),
                        ),
                    ] {
                        let source = format!(
                            "func value() -> Any {{ [:] }}\n\
                             func throwingValue() throws -> Any {{ [:] }}\n\
                             func consume<T>(_ value: T) {{}}\n{function}\n"
                        );
                        std::fs::write(&file, &source).expect("write Swift file");
                        let mut parser = FileParser::new();
                        let (tree, _) = parser.parse(&file).expect("parse Swift file");
                        checked += 1;
                        if tree.root_node().has_error() {
                            failures.push(format!(
                                "{context}: {expression}: {}",
                                tree.root_node().to_sexp()
                            ));
                        }
                    }
                }
            }
        }
    }
    assert_eq!(checked, 96);
    assert!(
        failures.is_empty(),
        "Swift cast/coalescing forms parsed with errors:\n{}",
        failures.join("\n")
    );
}

#[test]
fn swift_json_serialization_cast_and_split_workaround_have_no_parse_errors() {
    let tmp = crate::test_helpers::tempdir();
    let file = tmp.path().join("Transcript.swift");
    for body in [
        "let json = try JSONSerialization.jsonObject(with: data) as? [String: Any] ?? [:]",
        "let object = try JSONSerialization.jsonObject(with: data)\n\
         let json = (object as? [String: Any]) ?? [:]",
    ] {
        let source =
            format!("import Foundation\nfunc decode(data: Data) throws {{\n{body}\n_ = json\n}}\n");
        std::fs::write(&file, &source).expect("write Swift file");
        let mut parser = FileParser::new();
        let (tree, _) = parser.parse(&file).expect("parse Swift file");
        assert!(
            !tree.root_node().has_error(),
            "body {body:?} parsed with errors: {}",
            tree.root_node().to_sexp()
        );
    }
}

#[test]
fn swift_optional_binding_try_await_conditions_have_no_parse_errors() {
    let tmp = crate::test_helpers::tempdir();
    let file = tmp.path().join("ViewModel.swift");
    for condition in [
        "if let fresh = try? await fleet.fetch() {\n            rows = fresh.rows\n        }",
        "if ready, let fresh = try? await fleet.fetch() {\n            rows = fresh.rows\n        }",
        "guard let fresh = try? await fleet.fetch() else { return }\n        rows = fresh.rows",
        "while let fresh = try? await fleet.fetch() {\n            rows = fresh.rows\n        }",
    ] {
        let source = format!(
            "class ViewModel {{\n    func probe() async {{\n        {condition}\n    }}\n}}\n"
        );
        std::fs::write(&file, &source).expect("write Swift file");
        let mut parser = FileParser::new();
        let (tree, _) = parser.parse(&file).expect("parse Swift file");
        assert!(
            !tree.root_node().has_error(),
            "condition {condition:?} parsed with errors: {}",
            tree.root_node().to_sexp()
        );
    }
}

#[test]
fn python_decorated_function_range_includes_decorators() {
    let tmp = crate::test_helpers::tempdir();
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
fn symbol_cache_trusts_matching_size_and_mtime_until_invalidated() {
    // Freshness policy: a warm cache hit trusts matching size + mtime without
    // re-reading or hashing the file (the same trust the trigram index's
    // stat-first warm verification applies). A same-size edit that also
    // preserves mtime is therefore NOT detected by the freshness check alone —
    // only mtime-preserving tools (rsync --times, explicit utimes) produce
    // that shape, and the file watcher's cache reset is the invalidation
    // path that covers them.
    let tmp = crate::test_helpers::tempdir();
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
    assert!(
        second.iter().any(|symbol| symbol.name == "alpha"),
        "matching size+mtime is trusted: the cached symbols are served"
    );

    // Watcher-driven invalidation is the correctness path for
    // mtime-preserving edits: after it, the new content must be parsed.
    parser.invalidate_symbols(&file);
    let third = parser
        .extract_symbols(&file)
        .expect("extract third symbols");
    assert!(third.iter().any(|symbol| symbol.name == "bravo"));
    assert!(!third.iter().any(|symbol| symbol.name == "alpha"));
}

#[test]
fn symbol_cache_detects_mtime_moved_content_edit_of_same_size() {
    // The other direction stays guarded: when mtime moves, same-size content
    // changes must be detected (the content hash runs on the mtime-moved
    // path and catches them).
    let tmp = crate::test_helpers::tempdir();
    let file = tmp.path().join("cached.rs");
    std::fs::write(&file, "pub fn alpha() {}\n").expect("write rust file");

    let mut parser = FileParser::new();
    let first = parser
        .extract_symbols(&file)
        .expect("extract first symbols");
    assert!(first.iter().any(|symbol| symbol.name == "alpha"));

    std::fs::write(&file, "pub fn bravo() {}\n").expect("rewrite rust file same size");
    filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(4_102_444_800, 0))
        .expect("move mtime");

    let second = parser
        .extract_symbols(&file)
        .expect("extract second symbols");
    assert!(second.iter().any(|symbol| symbol.name == "bravo"));
    assert!(!second.iter().any(|symbol| symbol.name == "alpha"));
}

#[test]
fn ts_export_clause_marks_local_symbol_exported() {
    let tmp = crate::test_helpers::tempdir();
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
    let tmp = crate::test_helpers::tempdir();
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

#[test]
fn swift_try_await_control_flow_expressions_have_no_parse_errors() {
    let tmp = crate::test_helpers::tempdir();
    let file = tmp.path().join("ControlFlow.swift");
    let cases = [
        (
            "for record in try await pending()",
            "for record in try await pending() { #expect(record.id != nil) }",
        ),
        ("for x in try f()", "for x in try f() { use(x) }"),
        ("for x in await f()", "for x in await f() { use(x) }"),
        (
            "for try await x in seq",
            "for try await x in seq { use(x) }",
        ),
        ("while try await f()", "while try await f() { useValue() }"),
        (
            "guard let x = try await f() else",
            "guard let x = try await f() else { return }",
        ),
        (
            "switch try await f()",
            "switch try await f() { default: break }",
        ),
        (
            "for x in try? await f()",
            "for x in try? await f() { use(x) }",
        ),
        (
            "for x in try! await f()",
            "for x in try! await f() { use(x) }",
        ),
        (
            "while try? await f()",
            "while try? await f() { useValue() }",
        ),
        (
            "while try! await f()",
            "while try! await f() { useValue() }",
        ),
        (
            "guard let x = try? await f() else",
            "guard let x = try? await f() else { return }",
        ),
        (
            "guard let x = try! await f() else",
            "guard let x = try! await f() else { return }",
        ),
        (
            "switch try? await f()",
            "switch try? await f() { default: break }",
        ),
        (
            "switch try! await f()",
            "switch try! await f() { default: break }",
        ),
    ];
    let mut failures = Vec::new();

    for (form, body) in cases {
        let source = format!("@Test func consentRecords() async throws {{\n    {body}\n}}\n");
        std::fs::write(&file, &source).expect("write Swift file");
        let mut parser = FileParser::new();
        let (tree, _) = parser.parse(&file).expect("parse Swift file");
        if tree.root_node().has_error() {
            failures.push(format!("{form}: {}", tree.root_node().to_sexp()));
        }
    }

    assert!(
        failures.is_empty(),
        "Swift control-flow forms parsed with errors:\n{}",
        failures.join("\n")
    );
}
