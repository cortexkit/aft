use crate::helpers::AftProcess;
use std::fs;
use tempfile::TempDir;

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
    let dir = TempDir::new().unwrap();
    let md_file = dir.path().join("readme.md");
    fs::write(&md_file, SAMPLE_MD).unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(&format!(
        r#"{{"id":"md-1","command":"outline","file":"{}"}}"#,
        md_file.display()
    ));

    assert_eq!(resp["success"], true, "outline should succeed: {:?}", resp);

    let text = resp["text"]
        .as_str()
        .expect("text field should be a string");

    // Headings should use 'h' kind abbreviation
    assert!(text.contains(" h "), "headings should use 'h' kind");

    // All headings should be present in the outline
    assert!(text.contains("Project Title"), "should have Project Title");
    assert!(text.contains("Features"), "should have Features");
    assert!(text.contains("Sub-feature A"), "should have Sub-feature A");
    assert!(text.contains("Sub-feature B"), "should have Sub-feature B");
    assert!(text.contains("Architecture"), "should have Architecture");
    assert!(text.contains("Appendix"), "should have Appendix");

    // Exactly 2 top-level headings: lines with 2-space indent (starts with "  E " or "  - ")
    // and containing " h " kind
    let top_level_count = text
        .lines()
        .filter(|l| (l.starts_with("  E ") || l.starts_with("  - ")) && l.contains(" h "))
        .count();
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
    let dir = TempDir::new().unwrap();
    let md_file = dir.path().join("doc.md");
    fs::write(&md_file, SAMPLE_MD).unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(&format!(
        r#"{{"id":"md-2","command":"outline","file":"{}"}}"#,
        md_file.display()
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
    let dir = TempDir::new().unwrap();
    let md_file = dir.path().join("readme.md");
    fs::write(&md_file, SAMPLE_MD).unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(&format!(
        r#"{{"id":"md-3","command":"zoom","file":"{}","symbol":"Architecture"}}"#,
        md_file.display()
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
    let dir = TempDir::new().unwrap();
    let md_file = dir.path().join("readme.md");
    fs::write(&md_file, SAMPLE_MD).unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());

    let bare = aft.send(&format!(
        r#"{{"id":"md-prefix-bare","command":"zoom","file":"{}","symbol":"Architecture"}}"#,
        md_file.display()
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
            r#"{{"id":"{}","command":"zoom","file":"{}","symbol":"{}"}}"#,
            id,
            md_file.display(),
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
fn markdown_zoom_heading_not_found() {
    let dir = TempDir::new().unwrap();
    let md_file = dir.path().join("readme.md");
    fs::write(&md_file, SAMPLE_MD).unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(&format!(
        r#"{{"id":"md-4","command":"zoom","file":"{}","symbol":"Missing Heading"}}"#,
        md_file.display()
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
    let dir = TempDir::new().unwrap();
    let md_file = dir.path().join("new.md");

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());

    let new_content = "# New Doc\\n\\nSome content.\\n";
    let resp = aft.send(&format!(
        r#"{{"id":"md-5","command":"write","file":"{}","content":"{}"}}"#,
        md_file.display(),
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
    let dir = TempDir::new().unwrap();
    let mdx_file = dir.path().join("doc.mdx");
    fs::write(&mdx_file, "# MDX Doc\n\nSome content.\n").unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(&format!(
        r#"{{"id":"md-6","command":"outline","file":"{}"}}"#,
        mdx_file.display()
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
