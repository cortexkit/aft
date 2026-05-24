//! AST-grep language implementations for ast-grep-core.
//!
//! Provides `AstGrepLang` enum that implements `Language` and `LanguageExt`
//! traits from ast-grep-core, mapping to our tree-sitter language grammars.

use std::borrow::Cow;

use ast_grep_core::language::Language;
use ast_grep_core::matcher::PatternError;
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc, TSLanguage};
use ast_grep_core::Pattern;

use crate::parser::LangId;

/// Supported languages for AST pattern matching via ast-grep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AstGrepLang {
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Rust,
    Go,
    C,
    Cpp,
    Zig,
    CSharp,
    Solidity,
    Vue,
    Json,
    Java,
    Ruby,
    Kotlin,
    Swift,
    Php,
    Lua,
    Perl,
}

impl AstGrepLang {
    /// Convert from the crate's `LangId` enum.
    pub fn from_lang_id(lang_id: &LangId) -> Option<Self> {
        match lang_id {
            LangId::TypeScript => Some(Self::TypeScript),
            LangId::Tsx => Some(Self::Tsx),
            LangId::JavaScript => Some(Self::JavaScript),
            LangId::Python => Some(Self::Python),
            LangId::Rust => Some(Self::Rust),
            LangId::Go => Some(Self::Go),
            LangId::C => Some(Self::C),
            LangId::Cpp => Some(Self::Cpp),
            LangId::Zig => Some(Self::Zig),
            LangId::CSharp => Some(Self::CSharp),
            LangId::Solidity => Some(Self::Solidity),
            LangId::Vue => Some(Self::Vue),
            LangId::Json => Some(Self::Json),
            LangId::Java => Some(Self::Java),
            LangId::Ruby => Some(Self::Ruby),
            LangId::Kotlin => Some(Self::Kotlin),
            LangId::Swift => Some(Self::Swift),
            LangId::Php => Some(Self::Php),
            LangId::Lua => Some(Self::Lua),
            LangId::Perl => Some(Self::Perl),
            LangId::Scala => None,
            LangId::Bash => None, // ast-grep doesn't support Bash
            // Markdown, CSS, HTML etc. don't have meaningful AST patterns
            _ => None,
        }
    }

    /// Parse from a string (case-insensitive).
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "typescript" | "ts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "javascript" | "js" => Some(Self::JavaScript),
            "python" | "py" => Some(Self::Python),
            "rust" | "rs" => Some(Self::Rust),
            "go" | "golang" => Some(Self::Go),
            "c" => Some(Self::C),
            "cpp" | "c++" | "cplusplus" => Some(Self::Cpp),
            "zig" => Some(Self::Zig),
            "csharp" | "c#" | "cs" => Some(Self::CSharp),
            "solidity" | "sol" => Some(Self::Solidity),
            "vue" => Some(Self::Vue),
            "json" | "jsonc" => Some(Self::Json),
            "java" => Some(Self::Java),
            "ruby" | "rb" => Some(Self::Ruby),
            "kotlin" | "kt" | "kts" => Some(Self::Kotlin),
            "swift" => Some(Self::Swift),
            "php" => Some(Self::Php),
            "lua" => Some(Self::Lua),
            "perl" | "pl" | "pm" => Some(Self::Perl),
            _ => None,
        }
    }

    /// File extensions associated with this language.
    pub fn extensions(&self) -> &'static [&'static str] {
        match self {
            Self::TypeScript => &["ts", "mts", "cts"],
            Self::Tsx => &["tsx"],
            Self::JavaScript => &["js", "mjs", "cjs", "jsx"],
            Self::Python => &["py", "pyi"],
            Self::Rust => &["rs"],
            Self::Go => &["go"],
            Self::C => &["c", "h"],
            Self::Cpp => &["cc", "cpp", "cxx", "hpp", "hh"],
            Self::Zig => &["zig"],
            Self::CSharp => &["cs"],
            Self::Solidity => &["sol"],
            Self::Vue => &["vue"],
            Self::Json => &["json", "jsonc"],
            Self::Java => &["java"],
            Self::Ruby => &["rb"],
            Self::Kotlin => &["kt", "kts"],
            Self::Swift => &["swift"],
            Self::Php => &["php"],
            Self::Lua => &["lua"],
            Self::Perl => &["pl", "pm", "t"],
        }
    }

    /// Check if a file extension matches this language.
    pub fn matches_extension(&self, ext: &str) -> bool {
        self.extensions().contains(&ext)
    }

    /// Check if a file path matches this language based on its extension.
    pub fn matches_path(&self, path: &std::path::Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        self.matches_extension(ext)
    }
}

impl Language for AstGrepLang {
    fn kind_to_id(&self, kind: &str) -> u16 {
        let ts_lang: TSLanguage = self.get_ts_language();
        ts_lang.id_for_node_kind(kind, /* named */ true)
    }

    fn field_to_id(&self, field: &str) -> Option<u16> {
        self.get_ts_language()
            .field_id_for_name(field)
            .map(|f| f.get())
    }

    fn build_pattern(
        &self,
        builder: &ast_grep_core::matcher::PatternBuilder,
    ) -> Result<Pattern, PatternError> {
        builder.build(|src| StrDoc::try_new(src, self.clone()))
    }

    /// Some languages (Python, Rust) don't accept `$` as a valid identifier
    /// character. We replace `$` with the expando char (`µ`) in meta-variable
    /// positions so tree-sitter can parse the pattern as valid code.
    fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
        let expando = self.expando_char();
        if expando == '$' {
            // TS, JS, Go: $ is valid in identifiers, no preprocessing needed
            return Cow::Borrowed(query);
        }
        // Python, Rust: replace $ with µ in meta-variable positions
        // Logic from ast-grep's official pre_process_pattern
        let mut ret = Vec::with_capacity(query.len());
        let mut dollar_count = 0;
        for c in query.chars() {
            if c == '$' {
                dollar_count += 1;
                continue;
            }
            let need_replace = matches!(c, 'A'..='Z' | '_') || dollar_count == 3;
            let sigil = if need_replace { expando } else { '$' };
            ret.extend(std::iter::repeat(sigil).take(dollar_count));
            dollar_count = 0;
            ret.push(c);
        }
        // trailing anonymous multiple ($$$)
        let sigil = if dollar_count == 3 { expando } else { '$' };
        ret.extend(std::iter::repeat(sigil).take(dollar_count));
        Cow::Owned(ret.into_iter().collect())
    }

    fn expando_char(&self) -> char {
        match self {
            // $ is not a valid identifier char in Python, Rust, C-family, Zig, or C#.
            // Solidity intentionally allows `$` in identifiers
            // (`identifier: /[a-zA-Z$_][a-zA-Z0-9$_]*/` in tree-sitter-solidity),
            // so keep the meta-var sigil as `$` — replacing it with `µ`
            // breaks pattern parsing (µ is not in the Solidity identifier
            // character set, so `µNAME` is rejected by the grammar and
            // meta-vars never bind).
            Self::Python
            | Self::Rust
            | Self::C
            | Self::Cpp
            | Self::Zig
            | Self::CSharp
            | Self::Java
            | Self::Ruby
            | Self::Kotlin
            | Self::Swift
            | Self::Php
            | Self::Lua
            | Self::Perl => '\u{00B5}', // µ
            // $ is valid in TS, JS, Go, Solidity, and Vue template identifiers
            _ => '$',
        }
    }
}

impl LanguageExt for AstGrepLang {
    fn get_ts_language(&self) -> TSLanguage {
        match self {
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::Zig => tree_sitter_zig::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Solidity => tree_sitter_solidity::LANGUAGE.into(),
            Self::Vue => tree_sitter_vue::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_sg::LANGUAGE.into(),
            Self::Swift => tree_sitter_swift::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP_ONLY.into(),
            Self::Lua => tree_sitter_lua::LANGUAGE.into(),
            Self::Perl => tree_sitter_perl::LANGUAGE.into(),
        }
    }
}

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
        ];

        for (lang, source, pattern) in probes {
            let grep = lang.ast_grep(source);
            let root = grep.root();
            assert!(
                root.find(pattern).is_some(),
                "{lang:?} pattern should match: {pattern}"
            );
        }
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
