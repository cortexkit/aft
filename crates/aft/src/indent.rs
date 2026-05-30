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
            IndentStyle::Spaces(2) => "  ",
            IndentStyle::Spaces(4) => "    ",
            IndentStyle::Spaces(8) => "        ",
            IndentStyle::Spaces(n) => {
                // For uncommon widths, leak a static string. In practice
                // this only fires for exotic indent widths (1, 3, 5, 6, 7).
                let s: String = " ".repeat(*n as usize);
                Box::leak(s.into_boxed_str())
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
            | LangId::Yaml => IndentStyle::Spaces(2),
            LangId::Rust => IndentStyle::Spaces(4),
            LangId::Go => IndentStyle::Tabs,
            LangId::C | LangId::Cpp | LangId::Zig | LangId::CSharp | LangId::Bash => {
                IndentStyle::Spaces(4)
            }
            LangId::Solidity
            | LangId::Java
            | LangId::Kotlin
            | LangId::Swift
            | LangId::Php
            | LangId::Perl => IndentStyle::Spaces(4),
            LangId::Html => IndentStyle::Spaces(2),
            LangId::Markdown => IndentStyle::Spaces(4),
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
        if line.is_empty() {
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
        if matching > 0 && non_matching == 0 {
            return candidate;
        }
    }

    smallest as u8
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
    fn detect_indent_four_spaces_with_nested() {
        // Lines indented at 4 and 8 should detect 4-space indent
        let source = "impl Foo {\n    fn bar() {\n        let x = 1;\n    }\n}\n";
        assert_eq!(detect_indent(source, LangId::Rust), IndentStyle::Spaces(4));
    }
}
