//! Source fence for the SQLite file-set ownership rule. Compile-time source
//! inclusion avoids opening a database or any of its companion files to check it.

fn forbidden(source: &str, allow_text_controls: bool) -> bool {
    let raw = regex::Regex::new(
        r"(?:\bfs\s*::\s*(?:read|copy)|\bFile\s*::\s*open|\bOpenOptions\s*::\s*new)\s*\(",
    )
    .unwrap();
    let text = regex::Regex::new(r"\bfs\s*::\s*read_to_string\s*\(").unwrap();
    raw.is_match(source)
        || (!allow_text_controls && text.is_match(source))
        || source.contains("sqlite3_deserialize")
}

#[test]
fn native_sqlite_file_sets_have_no_out_of_band_content_access() {
    for (name, source, text_controls) in [
        ("v3.rs", include_str!("../v3.rs"), false),
        (
            "initialization.rs",
            include_str!("initialization.rs"),
            false,
        ),
        ("connections.rs", include_str!("connections.rs"), false),
        ("read_audit.rs", include_str!("read_audit.rs"), false),
        ("copy.rs", include_str!("copy.rs"), false),
        ("replay.rs", include_str!("replay.rs"), false),
        // Charges by path through getattrlist; it opens no file.
        ("unshared.rs", include_str!("unshared.rs"), false),
        // The process harness reads only its JSON/ready files and reader-floor
        // JSON as text. It must never read or copy SQLite file-set bytes.
        ("tests.rs", include_str!("tests.rs"), true),
        // Copy tests also read Rust sources as text to find callers.
        ("copy_tests.rs", include_str!("copy_tests.rs"), true),
        ("replay_tests.rs", include_str!("replay_tests.rs"), false),
        ("layout_tests.rs", include_str!("layout_tests.rs"), false),
        ("open_tests.rs", include_str!("open_tests.rs"), false),
        // Acquisition reads the pinned Cargo.lock payload as UTF-8 text only.
        ("layout_gate.rs", include_str!("layout_gate.rs"), true),
    ] {
        assert!(
            !forbidden(source, text_controls),
            "{name}: out-of-band SQLite file-set content access"
        );
    }
    let modules = regex::Regex::new(r"(?m)^mod\s+(\w+)\s*;").unwrap();
    for module in modules.captures_iter(include_str!("../v3.rs")) {
        assert!(
            [
                "connections",
                "initialization",
                "read_audit",
                "replay",
                "unshared",
                "copy",
                "copy_tests",
                "replay_tests",
                "tests",
                "source_guard",
                "layout_tests",
                "open_tests",
                "layout_gate"
            ]
            .contains(&&module[1]),
            "new v3 module must join the source fence: {}",
            &module[1]
        );
    }
}

#[test]
fn source_fence_recognizes_plain_descriptor_and_replay_regressions() {
    for code in [
        "std::fs::read(path)",
        "fs :: read (path)",
        "File::open(path)",
        "std::fs::copy(source, target)",
        "sqlite3_deserialize(handle)",
    ] {
        assert!(forbidden(code, false), "{code}");
    }
    assert!(!forbidden(
        "fs::read_dir(path); fs::symlink_metadata(path);",
        false
    ));
}
