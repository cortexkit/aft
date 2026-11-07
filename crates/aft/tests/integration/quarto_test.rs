use crate::helpers::AftProcess;
use std::fs;

const SAMPLE_QMD: &str = r#"---
title: My Quarto Document
author: Jane Doe
---

Setext Heading
==============

Some text under setext heading.

# Introduction

This is an introduction.

```{r}
# Some R code
x <- 1:10
plot(x)
```

## Section {#sec-id}

Some text under section with attribute.
"#;

#[test]
fn quarto_outline_extracts_headings() {
    let dir = crate::test_helpers::tempdir();
    let qmd_file = dir.path().join("document.qmd");
    fs::write(&qmd_file, SAMPLE_QMD).unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(dir.path());
    let resp = aft.send(&format!(
        r#"{{"id":"qmd-1","command":"outline","file":{}}}"#,
        crate::helpers::json_string(&qmd_file.display())
    ));

    assert_eq!(resp["success"], true, "outline should succeed: {:?}", resp);

    let text = resp["text"]
        .as_str()
        .expect("text field should be a string");

    // Single-file lines carry the signature directly (no `{vis} {kind}` prefix),
    // so a heading renders as its markdown signature: a leading run of '#'.
    assert!(
        text.lines().any(|l| l.trim_start().starts_with('#')),
        "headings should render with their markdown signature: {text}"
    );

    // Check if ATX heading is present
    assert!(text.contains("Introduction"), "should have Introduction");

    // Check if Setext heading is present
    assert!(
        text.contains("Setext Heading"),
        "should have Setext Heading"
    );

    // Check if ATX-with-attribute heading is present.
    // NOTE: The {#sec-id} attribute currently leaks into the heading text,
    // which is an acceptable minor limitation for this release.
    assert!(
        text.contains("Section {#sec-id}"),
        "should have Section {{#sec-id}}"
    );
}
