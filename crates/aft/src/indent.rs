//! Shared indentation detection utility (D042).
//!
//! Analyzes source file content to determine the indentation style (tabs vs
//! spaces, width) used. Falls back to language-specific defaults when the
//! file has insufficient indented lines or mixed signals.

use crate::parser::LangId;

/// Detected indentation style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndentStyle {
    Tabs,
    Spaces(u8),
}

impl IndentStyle {
    /// Returns the whitespace string for one level of this indent.
    pub fn as_str(&self) -> &'static str {
        match self {
            IndentStyle::Tabs => "\t",
            IndentStyle::Spaces(n) => {
                // Every u8 width fits in this shared ASCII buffer. Slicing it
                // keeps the static API without allocating or leaking per call.
                static SPACES: [u8; u8::MAX as usize] = [b' '; u8::MAX as usize];
                std::str::from_utf8(&SPACES[..*n as usize]).expect("ASCII spaces")
            }
        }
    }

    /// Language-specific default when detection has low confidence.
    pub fn default_for(lang: LangId) -> Self {
        match lang {
            LangId::Python => IndentStyle::Spaces(4),
            LangId::TypeScript
            | LangId::Tsx
            | LangId::JavaScript
            | LangId::Vue
            | LangId::Json
            | LangId::Scala
            | LangId::Ruby
            | LangId::Lua
            | LangId::Scss
            | LangId::Yaml
            | LangId::Toml => IndentStyle::Spaces(2),
            LangId::Rust => IndentStyle::Spaces(4),
            LangId::Go => IndentStyle::Tabs,
            LangId::C
            | LangId::Cpp
            | LangId::Cuda
            | LangId::Metal
            | LangId::Zig
            | LangId::CSharp
            | LangId::Bash => IndentStyle::Spaces(4),
            LangId::Solidity
            | LangId::ObjC
            | LangId::Java
            | LangId::Kotlin
            | LangId::Swift
            | LangId::Php
            | LangId::Perl
            | LangId::Groovy => IndentStyle::Spaces(4),
            LangId::Html => IndentStyle::Spaces(2),
            LangId::Markdown => IndentStyle::Spaces(4),
            LangId::Pascal | LangId::R => IndentStyle::Spaces(2),
        }
    }
}

/// Detect the indentation style of a source file.
///
/// Examines indented lines (those starting with whitespace) and determines
/// whether tabs or spaces dominate. For spaces, determines the most common
/// indent width by looking at the smallest indent unit.
///
/// Returns detected style if >50% of indented lines agree, otherwise falls
/// back to the language default.
pub fn detect_indent(source: &str, lang: LangId) -> IndentStyle {
    let mut tab_count: u32 = 0;
    let mut space_count: u32 = 0;
    let mut indent_widths: [u32; 9] = [0; 9]; // index 1..8

    for line in source.lines() {
        // The leading space in a block comment's ` *` decoration is not
        // evidence of the surrounding code's indentation unit.
        if line.trim().is_empty() || line.trim_start().starts_with('*') {
            continue;
        }
        let first = line.as_bytes()[0];
        if first == b'\t' {
            tab_count += 1;
        } else if first == b' ' {
            space_count += 1;
            // Count leading spaces
            let leading = line.len() - line.trim_start_matches(' ').len();
            if leading > 0 && leading <= 8 {
                indent_widths[leading] += 1;
            }
        }
    }

    let total = tab_count + space_count;
    if total == 0 {
        return IndentStyle::default_for(lang);
    }

    // Tabs win if >50% of indented lines use tabs
    if tab_count > total / 2 {
        return IndentStyle::Tabs;
    }

    // Spaces win if >50% of indented lines use spaces
    if space_count > total / 2 {
        // Determine the most likely indent unit width.
        // The unit is the GCD of observed indent widths, or equivalently,
        // the smallest width that has significant usage.
        let width = determine_space_width(&indent_widths);
        return IndentStyle::Spaces(width);
    }

    // Mixed / no clear winner — fall back
    IndentStyle::default_for(lang)
}

/// Determine the most likely space indent width from observed leading-space counts.
///
/// Strategy: find the smallest observed indent width that forms a consistent
/// pattern (all other widths are multiples of it). Prefer the smallest actual
/// indent seen, not just the GCD.
fn determine_space_width(widths: &[u32; 9]) -> u8 {
    // Find the smallest observed indent width
    let smallest = (1..=8usize).find(|&i| widths[i] > 0);
    let smallest = match smallest {
        Some(s) => s,
        None => return 4,
    };

    // Check if all observed widths are multiples of this smallest
    let all_multiples = (1..=8).all(|i| widths[i] == 0 || i % smallest == 0);

    if all_multiples && smallest >= 2 {
        return smallest as u8;
    }

    // If smallest is 1 or doesn't divide evenly, try common widths
    for &candidate in &[4u8, 2, 8] {
        let c = candidate as usize;
        let mut matching: u32 = 0;
        let mut non_matching: u32 = 0;
        for i in 1..=8 {
            if widths[i] > 0 {
                if i % c == 0 {
                    matching += widths[i];
                } else {
                    non_matching += widths[i];
                }
            }
        }
        if matching > non_matching {
            return candidate;
        }
    }

    smallest as u8
}

/// The line terminator generated text should use to match `source`: CRLF
/// when CRLF rows are at least as common as bare LF rows, LF otherwise.
/// Code inserted into a CRLF file must not introduce mixed line endings.
pub(crate) fn source_line_ending(source: &str) -> &'static str {
    let lf = source.bytes().filter(|byte| *byte == b'\n').count();
    let crlf = source
        .as_bytes()
        .windows(2)
        .filter(|pair| *pair == b"\r\n")
        .count();
    // `lf` counts every newline, including the ones that end CRLF rows.
    if crlf > 0 && crlf >= lf - crlf {
        "\r\n"
    } else {
        "\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_indent_tabs() {
        let source = "fn main() {\n\tlet x = 1;\n\tlet y = 2;\n}\n";
        assert_eq!(detect_indent(source, LangId::Rust), IndentStyle::Tabs);
    }

    #[test]
    fn detect_indent_two_spaces() {
        let source = "class Foo {\n  bar() {}\n  baz() {}\n}\n";
        assert_eq!(
            detect_indent(source, LangId::TypeScript),
            IndentStyle::Spaces(2)
        );
    }

    #[test]
    fn detect_indent_four_spaces() {
        let source =
            "class Foo:\n    def bar(self):\n        pass\n    def baz(self):\n        pass\n";
        assert_eq!(
            detect_indent(source, LangId::Python),
            IndentStyle::Spaces(4)
        );
    }

    #[test]
    fn detect_indent_empty_source_uses_default() {
        assert_eq!(detect_indent("", LangId::Python), IndentStyle::Spaces(4));
        assert_eq!(
            detect_indent("", LangId::TypeScript),
            IndentStyle::Spaces(2)
        );
        assert_eq!(detect_indent("", LangId::Go), IndentStyle::Tabs);
    }

    #[test]
    fn detect_indent_no_indented_lines_uses_default() {
        let source = "x = 1\ny = 2\n";
        assert_eq!(
            detect_indent(source, LangId::Python),
            IndentStyle::Spaces(4)
        );
    }

    #[test]
    fn indent_style_as_str() {
        assert_eq!(IndentStyle::Tabs.as_str(), "\t");
        assert_eq!(IndentStyle::Spaces(2).as_str(), "  ");
        assert_eq!(IndentStyle::Spaces(4).as_str(), "    ");
    }

    #[test]
    fn indent_strings_reuse_storage_for_every_width() {
        let mut addresses = std::collections::HashSet::new();
        for _ in 0..32 {
            for width in 0..=u8::MAX {
                let text = IndentStyle::Spaces(width).as_str();
                assert_eq!(text, " ".repeat(width as usize));
                addresses.insert((width, text.as_ptr() as usize));
            }
        }
        assert_eq!(
            addresses.len(),
            256,
            "one stable slice per width, not one allocation per call"
        );
    }

    #[test]
    fn detect_indent_four_spaces_with_nested() {
        // Lines indented at 4 and 8 should detect 4-space indent
        let source = "impl Foo {\n    fn bar() {\n        let x = 1;\n    }\n}\n";
        assert_eq!(detect_indent(source, LangId::Rust), IndentStyle::Spaces(4));
    }
}

#[cfg(test)]
mod audit11_regressions {
    use super::*;

    #[test]
    fn block_comment_decoration_does_not_override_code_indent() {
        let source = "/**\r\n * documentation\r\n */\r\nfunction run() {\r\n    one();\r\n    two();\r\n}\r\n";
        assert_eq!(
            detect_indent(source, LangId::TypeScript),
            IndentStyle::Spaces(4)
        );
        let mut widths = [0; 9];
        widths[1] = 1;
        widths[4] = 5;
        widths[8] = 2;
        assert_eq!(determine_space_width(&widths), 4);
        let generated = crate::extract::generate_extracted_function(
            "helper",
            &[],
            &crate::extract::ReturnKind::Void,
            "    one();\r\n    two();",
            "",
            LangId::TypeScript,
            detect_indent(source, LangId::TypeScript),
            source_line_ending(source),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let config = crate::config::Config {
            format_on_edit: false,
            ..Default::default()
        };
        crate::edit::write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            &generated,
            &config,
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(path).unwrap(),
            b"function helper() {\r\n    one();\r\n    two();\r\n}"
        );
    }
}
