pub use aft_parse::indent::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use crate::parser::LangId;

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
