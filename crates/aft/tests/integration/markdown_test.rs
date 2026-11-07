use crate::helpers::AftProcess;
use std::fs;

const SAMPLE_MD: &str = r#"# Project Title

Some introduction text.

## Features

- Feature one
- Feature two

### Sub-feature A

Details about sub-feature A.

### Sub-feature B

Details about sub-feature B.

## Architecture

The architecture section.

### Component X

Info about X.

# Appendix

Final notes.
"#;

#[test]
fn markdown_outline_extracts_headings() {
    let dir = crate::test_helpers::tempdir();
    let md_file = dir.path().join("readme.md");
    fs::write(&md_file, SAMPLE_MD).unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(&format!(
        r#"{{"id":"md-1","command":"outline","file":{}}}"#,
        crate::helpers::json_string(&md_file.display())
    ));

    assert_eq!(resp["success"], true, "outline should succeed: {:?}", resp);

    let text = resp["text"]
        .as_str()
        .expect("text field should be a string");

    // Single-file lines carry the signature directly (no `{vis} {kind}` prefix),
    // so a heading renders as its markdown signature: a leading run of '#'.
    assert!(
        text.contains("# Project Title"),
        "headings should render with their markdown signature: {text}"
    );

    // All headings should be present in the outline
    assert!(text.contains("Project Title"), "should have Project Title");
    assert!(text.contains("Features"), "should have Features");
    assert!(text.contains("Sub-feature A"), "should have Sub-feature A");
    assert!(text.contains("Sub-feature B"), "should have Sub-feature B");
    assert!(text.contains("Architecture"), "should have Architecture");
    assert!(text.contains("Appendix"), "should have Appendix");

    // Exactly 2 top-level headings: lines indented one level (two spaces) whose
    // signature starts with '#'. Nested headings are members and carry a '.' prefix.
    let top_level_count = text.lines().filter(|l| l.starts_with("  #")).count();
    assert_eq!(
        top_level_count, 2,
        "expected 2 top-level headings (Project Title and Appendix), got: {}",
        top_level_count
    );

    // Project Title and Appendix are top-level (their lines have no "." prefix)
    let project_title_line = text
        .lines()
        .find(|l| l.contains("Project Title"))
        .expect("Project Title line");
    assert!(
        !project_title_line.contains('.'),
        "Project Title should be top-level (no '.' prefix), got: {:?}",
        project_title_line
    );

    let appendix_line = text
        .lines()
        .find(|l| l.contains("Appendix"))
        .expect("Appendix line");
    assert!(
        !appendix_line.contains('.'),
        "Appendix should be top-level (no '.' prefix), got: {:?}",
        appendix_line
    );

    // Features and Architecture are nested (their lines have "." prefix)
    let features_line = text
        .lines()
        .find(|l| l.contains("Features"))
        .expect("Features line");
    assert!(
        features_line.contains('.'),
        "Features should be nested under Project Title (has '.' prefix), got: {:?}",
        features_line
    );

    let arch_line = text
        .lines()
        .find(|l| l.contains("Architecture"))
        .expect("Architecture line");
    assert!(
        arch_line.contains('.'),
        "Architecture should be nested under Project Title (has '.' prefix), got: {:?}",
        arch_line
    );
}

#[test]
fn markdown_outline_section_ranges_cover_content() {
    let dir = crate::test_helpers::tempdir();
    let md_file = dir.path().join("doc.md");
    fs::write(&md_file, SAMPLE_MD).unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(&format!(
        r#"{{"id":"md-2","command":"outline","file":{}}}"#,
        crate::helpers::json_string(&md_file.display())
    ));

    assert_eq!(resp["success"], true);

    let text = resp["text"]
        .as_str()
        .expect("text field should be a string");

    // Find the Features heading line to extract its range
    let features_line = text
        .lines()
        .find(|l| l.contains("Features"))
        .expect("Features should be in outline");

    // The range is the last token in the line, format "start:end"
    let range_part = features_line
        .split_whitespace()
        .last()
        .expect("range at end of line");
    let (start_str, end_str) = range_part
        .split_once(':')
        .expect("range should be in start:end format");
    let start: u64 = start_str.parse().expect("start should be a number");
    let end: u64 = end_str.parse().expect("end should be a number");

    // Section should cover multiple lines (heading + list + sub-sections)
    assert!(
        end > start + 3,
        "Features section should span multiple lines: {}-{}",
        start,
        end
    );
}

#[test]
fn markdown_zoom_by_heading_name() {
    let dir = crate::test_helpers::tempdir();
    let md_file = dir.path().join("readme.md");
    fs::write(&md_file, SAMPLE_MD).unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(&format!(
        r#"{{"id":"md-3","command":"zoom","file":{},"symbol":"Architecture"}}"#,
        crate::helpers::json_string(&md_file.display())
    ));

    assert_eq!(
        resp["success"], true,
        "zoom by heading name should work: {:?}",
        resp
    );
    let content = resp["content"].as_str().expect("content should be string");

    // Should contain the heading and its content
    assert!(
        content.contains("## Architecture"),
        "content should include heading"
    );
    assert!(
        content.contains("The architecture section"),
        "content should include body text"
    );
    assert!(
        content.contains("### Component X"),
        "content should include sub-heading"
    );
}

#[test]
fn markdown_zoom_accepts_copied_heading_prefixes() {
    let dir = crate::test_helpers::tempdir();
    let md_file = dir.path().join("readme.md");
    fs::write(&md_file, SAMPLE_MD).unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());

    let bare = aft.send(&format!(
        r#"{{"id":"md-prefix-bare","command":"zoom","file":{},"symbol":"Architecture"}}"#,
        crate::helpers::json_string(&md_file.display())
    ));
    assert_eq!(bare["success"], true, "bare heading should work: {bare:?}");

    for (id, symbol, expected) in [
        (
            "md-prefix-h2",
            "## Architecture",
            "The architecture section",
        ),
        (
            "md-prefix-h3",
            "### Sub-feature A",
            "Details about sub-feature A",
        ),
        ("md-prefix-h1", "# Project Title", "Some introduction text"),
    ] {
        let resp = aft.send(&format!(
            r#"{{"id":"{}","command":"zoom","file":{},"symbol":"{}"}}"#,
            id,
            crate::helpers::json_string(&md_file.display()),
            symbol
        ));
        assert_eq!(
            resp["success"], true,
            "prefixed markdown heading {symbol:?} should work: {resp:?}"
        );
        if symbol == "## Architecture" {
            assert_eq!(
                resp["range"], bare["range"],
                "## Architecture should match the bare heading range"
            );
        }
        assert!(
            resp["content"].as_str().unwrap().contains(expected),
            "content should include {expected:?}: {resp:?}"
        );
    }
}

#[test]
fn markdown_zoom_normalizes_heading_identity_variants() {
    let dir = crate::test_helpers::tempdir();
    let md_file = dir.path().join("decorated-headings.md");
    fs::write(
        &md_file,
        r#"# 12.1.5 Internationalization and links

Internationalization details.

## 📓 [Community Issue Tracker](https://example.com/issues)

Community tracker details.

## HTTP Flow

HTTP flow details.
"#,
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());

    for (id, symbol, expected) in [
        (
            "md-normalized-numeric",
            "Internationalization and links",
            "Internationalization details.",
        ),
        (
            "md-normalized-emoji-link",
            "Community Issue Tracker",
            "Community tracker details.",
        ),
        (
            "md-normalized-anchor-underscore",
            "#http_flow",
            "HTTP flow details.",
        ),
        (
            "md-normalized-anchor-hyphen",
            "http-flow",
            "HTTP flow details.",
        ),
    ] {
        let resp = aft.send(
            &serde_json::json!({
                "id": id,
                "command": "zoom",
                "file": md_file,
                "symbol": symbol,
            })
            .to_string(),
        );
        assert_eq!(resp["success"], true, "normalized heading zoom: {resp:?}");
        assert!(
            resp["content"].as_str().unwrap().contains(expected),
            "zoom should include {expected:?}: {resp:?}"
        );
    }

    let exact = aft.send(
        &serde_json::json!({
            "id": "md-normalized-exact",
            "command": "zoom",
            "file": md_file,
            "symbol": "📓 [Community Issue Tracker](https://example.com/issues)",
        })
        .to_string(),
    );
    assert_eq!(
        exact["success"], true,
        "raw heading should still match: {exact:?}"
    );
    assert_eq!(
        exact["name"],
        "📓 [Community Issue Tracker](https://example.com/issues)"
    );

    let suggestion = aft.send(
        &serde_json::json!({
            "id": "md-normalized-suggestion",
            "command": "zoom",
            "file": md_file,
            "symbol": "Community Issue Tracke",
        })
        .to_string(),
    );
    assert_eq!(
        suggestion["success"], false,
        "misspelled heading should fail"
    );
    let message = suggestion["message"].as_str().unwrap();
    // The steering contract: the refusal names the retry futility, offers the
    // heading under its DE-LINKED label, and never leaks the URL form.
    assert!(
        message.contains("Retrying this exact zoom call will fail again"),
        "refusal must carry the retry-unchanged signal: {message}"
    );
    assert!(
        message.contains("Community Issue Tracker"),
        "suggestion must offer the de-linked heading label: {message}"
    );
    assert!(!message.contains("[Community Issue Tracker](https://example.com/issues)"));

    assert!(aft.shutdown().success());
}

#[test]
fn markdown_zoom_normalization_collision_is_ambiguous() {
    let dir = crate::test_helpers::tempdir();
    let md_file = dir.path().join("ambiguous-headings.md");
    fs::write(
        &md_file,
        "# 1. Intro\n\nFirst intro.\n\n# 2. Intro\n\nSecond intro.\n",
    )
    .unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(
        &serde_json::json!({
            "id": "md-normalized-ambiguous",
            "command": "zoom",
            "file": md_file,
            "symbol": "Intro",
        })
        .to_string(),
    );

    assert_eq!(
        resp["success"], true,
        "collision should return a menu: {resp:?}"
    );
    assert_eq!(resp["kind"], "ambiguous_symbol");
    let content = resp["content"].as_str().unwrap();
    assert!(
        content.contains("1. Intro"),
        "raw candidate should be listed: {content}"
    );
    assert!(
        content.contains("2. Intro"),
        "raw candidate should be listed: {content}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn markdown_zoom_heading_not_found() {
    let dir = crate::test_helpers::tempdir();
    let md_file = dir.path().join("readme.md");
    fs::write(&md_file, SAMPLE_MD).unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(&format!(
        r#"{{"id":"md-4","command":"zoom","file":{},"symbol":"Missing Heading"}}"#,
        crate::helpers::json_string(&md_file.display())
    ));

    assert_eq!(
        resp["success"], false,
        "missing heading should error: {:?}",
        resp
    );
    assert_eq!(resp["code"], "symbol_not_found");
}

#[test]
fn markdown_write_preserves_content() {
    let dir = crate::test_helpers::tempdir();
    let md_file = dir.path().join("new.md");

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());

    let new_content = "# New Doc\\n\\nSome content.\\n";
    let resp = aft.send(&format!(
        r#"{{"id":"md-5","command":"write","file":{},"content":"{}"}}"#,
        crate::helpers::json_string(&md_file.display()),
        new_content
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    let on_disk = fs::read_to_string(&md_file).unwrap();
    assert!(
        on_disk.contains("# New Doc"),
        "written content should be on disk"
    );
}

#[test]
fn markdown_mdx_extension_supported() {
    let dir = crate::test_helpers::tempdir();
    let mdx_file = dir.path().join("doc.mdx");
    fs::write(&mdx_file, "# MDX Doc\n\nSome content.\n").unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(&format!(
        r#"{{"id":"md-6","command":"outline","file":{}}}"#,
        crate::helpers::json_string(&mdx_file.display())
    ));

    assert_eq!(resp["success"], true, "mdx should be supported: {:?}", resp);

    let text = resp["text"]
        .as_str()
        .expect("text field should be a string");
    assert!(
        text.contains("MDX Doc"),
        "outline should contain MDX Doc heading, got: {:?}",
        text
    );
}
