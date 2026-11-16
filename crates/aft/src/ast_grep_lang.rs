pub use aft_parse::ast_grep_lang::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use ast_grep_core::language::Language;
#[cfg(test)]
#[allow(unused_imports)]
use ast_grep_core::matcher::PatternError;
#[cfg(test)]
#[allow(unused_imports)]
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc, TSLanguage};
#[cfg(test)]
#[allow(unused_imports)]
use ast_grep_core::Pattern;
#[cfg(test)]
#[allow(unused_imports)]
use std::borrow::Cow;

#[cfg(test)]
mod tests {
    use super::*;
    use ast_grep_core::tree_sitter::LanguageExt;

    #[test]
    fn test_from_str() {
        assert_eq!(
            AstGrepLang::from_str("typescript"),
            Some(AstGrepLang::TypeScript)
        );
        assert_eq!(AstGrepLang::from_str("tsx"), Some(AstGrepLang::Tsx));
        assert_eq!(
            AstGrepLang::from_str("javascript"),
            Some(AstGrepLang::JavaScript)
        );
        assert_eq!(AstGrepLang::from_str("python"), Some(AstGrepLang::Python));
        assert_eq!(AstGrepLang::from_str("rust"), Some(AstGrepLang::Rust));
        assert_eq!(AstGrepLang::from_str("go"), Some(AstGrepLang::Go));
        assert_eq!(AstGrepLang::from_str("c"), Some(AstGrepLang::C));
        assert_eq!(AstGrepLang::from_str("cpp"), Some(AstGrepLang::Cpp));
        assert_eq!(AstGrepLang::from_str("cuda"), Some(AstGrepLang::Cuda));
        assert_eq!(AstGrepLang::from_str("metal"), Some(AstGrepLang::Metal));
        assert_eq!(AstGrepLang::from_str("zig"), Some(AstGrepLang::Zig));
        assert_eq!(AstGrepLang::from_str("c#"), Some(AstGrepLang::CSharp));
        assert_eq!(
            AstGrepLang::from_str("solidity"),
            Some(AstGrepLang::Solidity)
        );
        assert_eq!(AstGrepLang::from_str("sol"), Some(AstGrepLang::Solidity));
        assert_eq!(AstGrepLang::from_str("vue"), Some(AstGrepLang::Vue));
        assert_eq!(AstGrepLang::from_str("json"), Some(AstGrepLang::Json));
        assert_eq!(AstGrepLang::from_str("java"), Some(AstGrepLang::Java));
        assert_eq!(AstGrepLang::from_str("ruby"), Some(AstGrepLang::Ruby));
        assert_eq!(AstGrepLang::from_str("kotlin"), Some(AstGrepLang::Kotlin));
        assert_eq!(AstGrepLang::from_str("swift"), Some(AstGrepLang::Swift));
        assert_eq!(AstGrepLang::from_str("php"), Some(AstGrepLang::Php));
        assert_eq!(AstGrepLang::from_str("lua"), Some(AstGrepLang::Lua));
        assert_eq!(AstGrepLang::from_str("perl"), Some(AstGrepLang::Perl));
        assert_eq!(AstGrepLang::from_str("pascal"), Some(AstGrepLang::Pascal));
        assert_eq!(AstGrepLang::from_str("r"), Some(AstGrepLang::R));
        assert_eq!(AstGrepLang::from_str("groovy"), Some(AstGrepLang::Groovy));
        assert_eq!(AstGrepLang::from_str("objc"), Some(AstGrepLang::ObjC));
        assert_eq!(
            AstGrepLang::from_str("objective-c"),
            Some(AstGrepLang::ObjC)
        );
        assert_eq!(AstGrepLang::from_str("markdown"), None);
    }

    #[test]
    fn test_ast_grep_basic() {
        let lang = AstGrepLang::TypeScript;
        let grep = lang.ast_grep("const x = 1;");
        let root = grep.root();
        assert!(root.find("const $X = $Y").is_some());
    }

    #[test]
    fn test_python_function_pattern() {
        let lang = AstGrepLang::Python;
        let source = "def add(a, b):\n    return a + b\n";
        let grep = lang.ast_grep(source);
        let root = grep.root();
        // Pattern with meta-variables — $ gets replaced with µ for parsing
        let found = root.find("def $FUNC($$$):\n    return $X");
        assert!(found.is_some(), "Python function pattern should match");
        let node = found.unwrap();
        assert_eq!(node.text(), "def add(a, b):\n    return a + b");
    }

    #[test]
    fn test_python_expression_pattern() {
        let lang = AstGrepLang::Python;
        let source = "x = self.value + 1\n";
        let grep = lang.ast_grep(source);
        let root = grep.root();
        let found = root.find("self.$ATTR + $X");
        assert!(found.is_some(), "Python expression pattern should match");
    }

    #[test]
    fn test_rust_function_pattern() {
        let lang = AstGrepLang::Rust;
        let source = "fn add(a: i32, b: i32) -> i32 { a + b }";
        let grep = lang.ast_grep(source);
        let root = grep.root();
        let found = root.find("fn $NAME($$$) -> $RET { $$$BODY }");
        assert!(found.is_some(), "Rust function pattern should match");
    }

    #[test]
    fn test_new_language_ast_grep_pattern_probes() {
        let probes = [
            (
                AstGrepLang::Cuda,
                "__global__ void transform(float *data) { data[0] = 1.0f; }",
                "__global__ void transform(float *data) { data[0] = 1.0f; }",
            ),
            (
                AstGrepLang::Metal,
                "kernel void transform(device float *data) { data[0] = 1.0f; }",
                "kernel void transform(device float *data) { data[0] = 1.0f; }",
            ),
            (
                AstGrepLang::Java,
                "class Greeter { String greet(String who) { return who; } }",
                "class $NAME { $$$ }",
            ),
            (
                AstGrepLang::Ruby,
                "class Greeter\n  def greet(who)\n    who\n  end\nend\n",
                "def $METHOD($$$)\n  $$$\nend",
            ),
            (
                AstGrepLang::Kotlin,
                "class Greeter { fun greet(who: String): String = who }",
                "fun $NAME($$$): $RET = $BODY",
            ),
            (
                AstGrepLang::Swift,
                "func greet(_ who: String) -> String { return who }",
                "func greet(_ who: String) -> String { return who }",
            ),
            (
                AstGrepLang::Php,
                "<?php\nfunction helper(): void {}\nhelper();\n",
                "function $NAME(): void {}",
            ),
            (
                AstGrepLang::Lua,
                "function greet(name)\n  return name\nend\n",
                "function $NAME($$$)\n  $$$\nend",
            ),
            (
                AstGrepLang::Perl,
                "sub greet { return 1; }\ngreet();\n",
                "sub greet { return 1; }",
            ),
            (
                AstGrepLang::Pascal,
                "procedure SayHello(name: string);\nbegin\n  writeln(name);\nend;",
                "procedure $NAME($$$);",
            ),
            (
                AstGrepLang::R,
                "result <- sum(values)\n",
                "$NAME <- sum($VALUES)",
            ),
            (
                AstGrepLang::Groovy,
                "def greet(name) { println \"Hello, $name\" }\n",
                "def $NAME($$$PARAMS) { $$$BODY }",
            ),
            (
                AstGrepLang::ObjC,
                "id result = [obj foo:bar];\n",
                "[obj foo:$ARG]",
            ),
        ];

        for (lang, source, pattern) in probes {
            let grep = lang.ast_grep(source);
            let root = grep.root();
            let compiled = lang
                .compile_pattern(pattern)
                .unwrap_or_else(|err| panic!("{lang:?} pattern should compile: {err}"));
            assert!(
                root.find(compiled).is_some(),
                "{lang:?} pattern should match: {pattern}"
            );
        }
    }

    #[test]
    fn test_php_full_file_with_open_tag_and_inline_html_matches_snippet_pattern() {
        let lang = AstGrepLang::Php;
        let source = "<p>before</p>
<?php
function helper(): void {}
helper();
?>
<p>after</p>
";
        let grep = lang.ast_grep(source);
        let root = grep.root();
        let pattern = lang.compile_pattern("function $NAME(): void {}").unwrap();
        let found = root.find(pattern);
        assert!(
            found.is_some(),
            "PHP snippet pattern should match a full .php file with tags"
        );
        assert_eq!(
            found.unwrap().get_env().get_match("NAME").unwrap().text(),
            "helper"
        );
    }

    #[test]
    fn test_expando_char() {
        assert_eq!(AstGrepLang::Python.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::Rust.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::C.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::Cpp.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::Zig.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::CSharp.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::Java.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::Ruby.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::Kotlin.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::Swift.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::Php.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::Lua.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::Perl.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::R.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::ObjC.expando_char(), '\u{00B5}');
        assert_eq!(AstGrepLang::Pascal.expando_char(), '_');
        assert_eq!(AstGrepLang::TypeScript.expando_char(), '$');
        assert_eq!(AstGrepLang::JavaScript.expando_char(), '$');
        assert_eq!(AstGrepLang::Go.expando_char(), '$');
    }

    #[test]
    fn test_solidity_function_pattern_probe() {
        let lang = AstGrepLang::Solidity;
        let source = "contract C {\n    function add(uint256 a) public pure returns (uint256) { return a; }\n}\n";
        let grep = lang.ast_grep(source);
        let root = grep.root();
        // Probe several pattern shapes; print which ones tree-sitter-solidity
        // accepts. At least one MUST match — that proves grammar wiring works
        // end to end. ast-grep pattern shape varies by language so we don't
        // pin to one specific pattern.
        let patterns = [
            "return $X;",
            "uint256 $X",
            "function $NAME",
            "function add",
            "contract C { $$$ }",
        ];
        let mut any_matched = false;
        for pat in &patterns {
            if root.find(*pat).is_some() {
                any_matched = true;
                break;
            }
        }
        assert!(
            any_matched,
            "no Solidity pattern matched — grammar wiring broken"
        );
    }

    /// Solidity grammar permits `$` in identifiers
    /// (`/[a-zA-Z$_][a-zA-Z0-9$_]*/`), so `expando_char` must stay `$` —
    /// not `µ` like other non-`$` languages. If `µ` were used, `$NAME` →
    /// `µNAME`, which the Solidity grammar rejects (µ is outside the
    /// identifier character set), and meta-vars never bind. Pin the
    /// expected behavior so the bug we fixed in v0.19.5 cannot silently
    /// regress.
    #[test]
    fn solidity_expando_char_stays_dollar() {
        assert_eq!(AstGrepLang::Solidity.expando_char(), '$');
    }

    #[test]
    fn vue_expando_char_stays_dollar() {
        assert_eq!(AstGrepLang::Vue.expando_char(), '$');
    }

    #[test]
    fn groovy_expando_char_stays_dollar() {
        assert_eq!(AstGrepLang::Groovy.expando_char(), '$');
    }

    /// Regression for the Solidity meta-var binding bug that v0.19.5 fixed.
    /// Before the fix, every `$NAME` in a Solidity pattern was rewritten to
    /// `µNAME`, and the grammar rejected the result, so meta-vars never
    /// bound and `total_matches` was always 0 for any pattern using
    /// meta-vars.
    #[test]
    fn solidity_meta_var_pattern_binds_capture() {
        let lang = AstGrepLang::Solidity;
        let source =
            "contract C {\n    function add(uint256 a) public pure returns (uint256) { return a; }\n}\n";
        let grep = lang.ast_grep(source);
        let root = grep.root();
        let found = root.find("function $NAME($$$) public pure returns ($$$) { $$$ }");
        assert!(
            found.is_some(),
            "Solidity meta-var pattern must match — bug recurrence"
        );
    }

    #[test]
    fn pascal_meta_var_pattern_binds_capture() {
        let lang = AstGrepLang::Pascal;
        let source = "procedure SayHello(name: string);\nbegin\n  writeln(name);\nend;";
        let grep = lang.ast_grep(source);
        let root = grep.root();
        let found = root.find("procedure $NAME($$$);");
        assert!(
            found.is_some(),
            "Pascal meta-var pattern must match — bug recurrence"
        );
        assert_eq!(
            found.unwrap().get_env().get_match("NAME").unwrap().text(),
            "SayHello"
        );
    }

    #[test]
    fn r_meta_var_pattern_uses_micro_expando_and_binds_capture() {
        let lang = AstGrepLang::R;
        let source = "result <- sum(values)\n";
        let grep = lang.ast_grep(source);
        let root = grep.root();
        let found = root.find("$NAME <- sum($VALUES)");
        assert!(
            found.is_some(),
            "R meta-var pattern must match — bug recurrence"
        );
        let found = found.unwrap();
        let env = found.get_env();
        assert_eq!(env.get_match("NAME").unwrap().text(), "result");
        assert_eq!(env.get_match("VALUES").unwrap().text(), "values");
    }

    #[test]
    fn r_micro_expando_parses_as_identifier_token() {
        let lang = AstGrepLang::R;
        let processed = lang.pre_process_pattern("$NAME <- $VALUE");
        assert!(
            processed.starts_with('\u{00B5}'),
            "R meta-var should use µ expando, got {processed:?}"
        );
        let grep = lang.ast_grep(processed.as_ref());
        let root = grep.root();
        let assignment = root
            .children()
            .into_iter()
            .find(|child| child.kind() == "binary_operator")
            .expect("processed R assignment should parse as binary_operator");
        let identifier = assignment
            .children()
            .into_iter()
            .find(|child| child.kind() == "identifier")
            .expect("µNAME should parse as one identifier token");
        assert_eq!(identifier.text(), "µNAME");
    }

    #[test]
    fn groovy_meta_var_pattern_uses_micro_expando_and_binds_capture() {
        let lang = AstGrepLang::Groovy;
        let source = "class Greeter { def greet(name) { println \"Hello, $name\" } }\n";
        let grep = lang.ast_grep(source);
        let root = grep.root();
        let processed = lang.pre_process_pattern("class $NAME { $$$BODY }");
        assert!(
            processed.contains("$NAME"),
            "Groovy meta-var should keep the $ sigil, got {processed:?}"
        );
        let found = root.find("class $NAME { $$$BODY }");
        assert!(
            found.is_some(),
            "Groovy meta-var pattern must match class declaration"
        );
        let found = found.unwrap();
        let env = found.get_env();
        assert_eq!(env.get_match("NAME").unwrap().text(), "Greeter");
    }

    #[test]
    fn groovy_gstring_literal_pattern_parses_and_rewrites() {
        let lang = AstGrepLang::Groovy;
        let source = "class Greeter { def greet(name) { println \"Hello, $name\" } }\n";
        let grep = lang.ast_grep(source);
        let root = grep.root();

        let gstring_pattern = lang
            .compile_pattern("println \"Hello, $name\"")
            .expect("Groovy GString literal pattern should compile");
        assert!(
            root.find(gstring_pattern).is_some(),
            "Groovy GString literal pattern should match"
        );

        let rename_pattern = lang
            .compile_pattern("class $NAME { $$$BODY }")
            .expect("Groovy class pattern should compile");
        let mut edits = grep
            .root()
            .replace_all(&rename_pattern, "class Messenger { $$$BODY }");
        edits.sort_by(|a, b| b.position.cmp(&a.position));
        let mut rewritten = source.as_bytes().to_vec();
        for edit in &edits {
            let start = edit.position;
            let end = start + edit.deleted_length;
            rewritten.splice(start..end, edit.inserted_text.iter().copied());
        }
        let rewritten = String::from_utf8(rewritten).expect("Groovy rewrite stays UTF-8");
        assert!(rewritten.contains("class Messenger"));
        assert!(rewritten.contains("\"Hello, $name\""));
    }

    #[test]
    fn groovy_matches_gradle_and_jenkins_paths() {
        assert!(AstGrepLang::Groovy.matches_path(std::path::Path::new("build.gradle")));
        assert!(AstGrepLang::Groovy.matches_path(std::path::Path::new("Jenkinsfile")));
        assert!(!AstGrepLang::Groovy.matches_path(std::path::Path::new("build.gradle.kts")));
    }

    #[test]
    fn objc_meta_var_pattern_uses_micro_expando_and_binds_capture() {
        let lang = AstGrepLang::ObjC;
        let source = "id result = [obj foo:bar];\n";
        let grep = lang.ast_grep(source);
        let root = grep.root();
        let found = root.find("[obj foo:$ARG]");
        assert!(
            found.is_some(),
            "Objective-C meta-var pattern must match message expression"
        );
        assert_eq!(
            found.unwrap().get_env().get_match("ARG").unwrap().text(),
            "bar"
        );
    }

    #[test]
    fn objc_micro_expando_parses_as_identifier_token() {
        let lang = AstGrepLang::ObjC;
        let processed = lang.pre_process_pattern("[obj foo:$ARG]");
        assert!(
            processed.contains('\u{00B5}'),
            "Objective-C meta-var should use µ expando, got {processed:?}"
        );
        let grep = lang.ast_grep(processed.as_ref());
        let root = grep.root();
        let mut stack = root.children().collect::<Vec<_>>();
        let mut saw_expando_identifier = false;
        while let Some(node) = stack.pop() {
            if node.kind() == "identifier" && node.text() == "µARG" {
                saw_expando_identifier = true;
                break;
            }
            stack.extend(node.children());
        }
        assert!(
            saw_expando_identifier,
            "µARG should parse as one Objective-C identifier token"
        );
    }

    #[test]
    fn test_pre_process_pattern_python() {
        let lang = AstGrepLang::Python;
        let result = lang.pre_process_pattern("def $FUNC($$$):");
        // $ before uppercase or $$$ should be replaced with µ
        assert!(result.contains('\u{00B5}'), "Should contain µ expando char");
        assert!(
            !result.contains('$'),
            "Should not contain $ after preprocessing"
        );
    }
}

#[cfg(test)]
mod audit11_regressions {
    use super::*;

    #[test]
    fn pascal_declaration_keywords_have_identifier_boundaries() {
        for expression in [
            "variable := 1;",
            "typeinfo(x);",
            "functionality(x);",
            "unit_test();",
        ] {
            assert!(!pascal_pattern_is_declaration(expression), "{expression}");
        }
        for declaration in ["var x: integer;", "FUNCTION Foo;", "type T = integer;"] {
            assert!(pascal_pattern_is_declaration(declaration), "{declaration}");
        }
        let source = "\u{feff}program Demo;\r\nvar variable: Integer;\r\nbegin\r\n  variable := 1;\r\nend.\r\n";
        let lang = AstGrepLang::Pascal;
        let pattern = lang.compile_pattern("variable := 1;").unwrap();
        let grep = lang.ast_grep(source);
        let edits = grep.root().replace_all(&pattern, "variable := 2");
        assert_eq!(edits.len(), 1);
        let mut rewritten = source.as_bytes().to_vec();
        for edit in edits {
            rewritten.splice(
                edit.position..edit.position + edit.deleted_length,
                edit.inserted_text,
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let config = crate::config::Config {
            format_on_edit: false,
            ..Default::default()
        };
        crate::edit::write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            std::str::from_utf8(&rewritten).unwrap(),
            &config,
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), "\u{feff}program Demo;\r\nvar variable: Integer;\r\nbegin\r\n  variable := 2;\r\nend.\r\n".as_bytes());
    }
}
