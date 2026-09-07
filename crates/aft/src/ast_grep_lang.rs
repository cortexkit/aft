//! AST-grep language implementations for ast-grep-core.
//!
//! Provides `AstGrepLang` enum that implements `Language` and `LanguageExt`
//! traits from ast-grep-core, mapping to our tree-sitter language grammars.

use std::borrow::Cow;

use ast_grep_core::language::Language;
use ast_grep_core::matcher::PatternError;
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc, TSLanguage};
use ast_grep_core::Pattern;

/// Supported languages for AST pattern matching via ast-grep.
///
/// TOML remains outside this enum, like YAML: its data-oriented grammar does
/// not accept ast-grep's identifier metavariable sentinels as keys.
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
    Cuda,
    Metal,
    Zig,
    CSharp,
    Solidity,
    Scss,
    Vue,
    Json,
    Java,
    Ruby,
    Kotlin,
    Swift,
    Php,
    Lua,
    Perl,
    Pascal,
    R,
    Groovy,
    ObjC,
}

#[derive(Clone, Debug)]
enum PhpPatternLang {
    Full,
    Snippet,
}

impl PhpPatternLang {
    fn pattern_has_open_tag(source: &str) -> bool {
        source.contains("<?php") || source.contains("<?=")
    }
}

impl Language for PhpPatternLang {
    fn kind_to_id(&self, kind: &str) -> u16 {
        let ts_lang: TSLanguage = self.get_ts_language();
        ts_lang.id_for_node_kind(kind, true)
    }

    fn field_to_id(&self, field: &str) -> Option<u16> {
        self.get_ts_language()
            .field_id_for_name(field)
            .map(|field| field.get())
    }

    fn build_pattern(
        &self,
        builder: &ast_grep_core::matcher::PatternBuilder,
    ) -> Result<Pattern, PatternError> {
        builder.build(|src| StrDoc::try_new(src, self.clone()))
    }

    fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
        pre_process_pattern_with_expando(query, self.expando_char())
    }

    fn expando_char(&self) -> char {
        '\u{00B5}'
    }
}

impl LanguageExt for PhpPatternLang {
    fn get_ts_language(&self) -> TSLanguage {
        match self {
            Self::Full => tree_sitter_php::LANGUAGE_PHP.into(),
            Self::Snippet => tree_sitter_php::LANGUAGE_PHP_ONLY.into(),
        }
    }
}

impl AstGrepLang {
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
            "cuda" | "cu" => Some(Self::Cuda),
            "metal" => Some(Self::Metal),
            "zig" => Some(Self::Zig),
            "csharp" | "c#" | "cs" => Some(Self::CSharp),
            "solidity" | "sol" => Some(Self::Solidity),
            "scss" => Some(Self::Scss),
            "vue" => Some(Self::Vue),
            "json" | "jsonc" => Some(Self::Json),
            "java" => Some(Self::Java),
            "ruby" | "rb" => Some(Self::Ruby),
            "kotlin" | "kt" | "kts" => Some(Self::Kotlin),
            "swift" => Some(Self::Swift),
            "php" => Some(Self::Php),
            "lua" => Some(Self::Lua),
            "perl" | "pl" | "pm" => Some(Self::Perl),
            "pascal" | "pas" | "pp" | "dpr" | "dpk" | "lpr" => Some(Self::Pascal),
            "r" => Some(Self::R),
            "groovy" => Some(Self::Groovy),
            "objc" | "objective-c" | "objectivec" | "obj-c" => Some(Self::ObjC),
            _ => None,
        }
    }

    pub(crate) fn compile_pattern(&self, pattern: &str) -> Result<Pattern, PatternError> {
        if matches!(self, Self::Pascal) {
            if pascal_pattern_is_declaration(pattern) {
                return Pattern::try_new(pattern, self.clone());
            }
            let selector = pascal_snippet_selector(pattern)?;
            let context = format!("procedure dummy; begin {pattern} end;");
            return Pattern::contextual(&context, &selector, self.clone());
        }

        if !matches!(self, Self::Php) {
            return Pattern::try_new(pattern, self.clone());
        }

        if PhpPatternLang::pattern_has_open_tag(pattern) {
            return Pattern::try_new(pattern, PhpPatternLang::Full);
        }

        let selector = php_snippet_selector(pattern)?;
        let context = format!("<?php\n{pattern}");
        Pattern::contextual(&context, &selector, PhpPatternLang::Full)
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
            Self::Cuda => &["cu", "cuh"],
            Self::Metal => &["metal"],
            Self::Zig => &["zig"],
            Self::CSharp => &["cs"],
            Self::Solidity => &["sol"],
            Self::Scss => &["scss"],
            Self::Vue => &["vue"],
            Self::Json => &["json", "jsonc"],
            Self::Java => &["java"],
            Self::Ruby => &["rb"],
            Self::Kotlin => &["kt", "kts"],
            Self::Swift => &["swift"],
            Self::Php => &["inc", "php"],
            Self::Lua => &["lua"],
            Self::Perl => &["pl", "pm", "t"],
            Self::Pascal => &["pas", "pp", "dpr", "dpk", "lpr"],
            Self::R => &["R", "r"],
            Self::Groovy => &["groovy", "gvy", "gy", "gsh", "gradle"],
            Self::ObjC => &["m", "mm"],
        }
    }

    /// Check if a file extension matches this language.
    pub fn matches_extension(&self, ext: &str) -> bool {
        self.extensions().contains(&ext)
    }

    /// Check if a file path matches this language based on its extension.
    pub fn matches_path(&self, path: &std::path::Path) -> bool {
        if matches!(self, Self::Groovy)
            && path.file_name().and_then(|name| name.to_str()) == Some("Jenkinsfile")
        {
            return true;
        }

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        self.matches_extension(ext)
    }
}

fn php_snippet_selector(pattern: &str) -> Result<String, PatternError> {
    Pattern::try_new(pattern, PhpPatternLang::Snippet)?;
    let processed = PhpPatternLang::Snippet.pre_process_pattern(pattern);
    let grep = PhpPatternLang::Snippet.ast_grep(processed.as_ref());
    let mut node = grep.root();
    while node.children().len() == 1 {
        node = node.child(0).expect("single child exists");
    }
    Ok(node.kind().into_owned())
}

fn pascal_pattern_is_declaration(pattern: &str) -> bool {
    let trimmed = pattern.trim_start().to_lowercase();
    trimmed.starts_with("program")
        || trimmed.starts_with("unit")
        || trimmed.starts_with("procedure")
        || trimmed.starts_with("function")
        || trimmed.starts_with("constructor")
        || trimmed.starts_with("destructor")
        || trimmed.starts_with("type")
        || trimmed.starts_with("const")
        || trimmed.starts_with("var")
}

fn pascal_snippet_selector(pattern: &str) -> Result<String, PatternError> {
    let context = format!("procedure dummy; begin {pattern} end;");
    let grep = AstGrepLang::Pascal.ast_grep(context);
    let root = grep.root();
    let mut node = root;
    while node.children().len() == 1 {
        node = node.child(0).expect("single child exists");
    }
    let mut block_node = None;
    for child in node.children() {
        if child.kind() == "block" {
            block_node = Some(child);
            break;
        }
    }
    let Some(block) = block_node else {
        return Err(PatternError::NoContent(
            "invalid Pascal pattern structure".to_string(),
        ));
    };
    let mut stmt_node = None;
    for child in block.children() {
        if child.kind() != "kBegin" && child.kind() != "kEnd" {
            stmt_node = Some(child);
            break;
        }
    }
    let Some(stmt) = stmt_node else {
        return Err(PatternError::NoContent("empty Pascal pattern".to_string()));
    };
    Ok(stmt.kind().into_owned())
}

fn pre_process_pattern_with_expando<'q>(query: &'q str, expando: char) -> Cow<'q, str> {
    if expando == '$' {
        // TS, JS, Go: $ is valid in identifiers, no preprocessing needed
        return Cow::Borrowed(query);
    }
    // Python, Rust, PHP, and other grammars that reject `$` identifiers:
    // replace $ with µ in meta-variable positions. Logic from ast-grep's
    // official pre_process_pattern.
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
        if matches!(self, Self::Php) {
            return builder.build(|src| StrDoc::try_new(src, PhpPatternLang::Snippet));
        }

        builder.build(|src| StrDoc::try_new(src, self.clone()))
    }

    /// Some languages (Python, Rust) don't accept `$` as a valid identifier
    /// character. We replace `$` with the expando char (`µ`) in meta-variable
    /// positions so tree-sitter can parse the pattern as valid code.
    fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
        pre_process_pattern_with_expando(query, self.expando_char())
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
            | Self::Cuda
            | Self::Metal
            | Self::Zig
            | Self::CSharp
            | Self::Java
            | Self::Ruby
            | Self::Kotlin
            | Self::Swift
            | Self::Php
            | Self::Lua
            | Self::Perl
            | Self::R
            | Self::ObjC => '\u{00B5}', // µ
            Self::Pascal => '_',
            // $ is valid in TS, JS, Go, Solidity, Groovy, and Vue template identifiers.
            // Groovy also uses $ inside GStrings, so keeping $ avoids rejecting
            // literal interpolation patterns while still letting $NAME meta-vars bind.
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
            Self::Cuda => tree_sitter_cuda::LANGUAGE.into(),
            Self::Metal => tree_sitter_cpp::LANGUAGE.into(),
            Self::Zig => tree_sitter_zig::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Solidity => tree_sitter_solidity::LANGUAGE.into(),
            Self::Scss => tree_sitter_scss::language(),
            Self::Vue => tree_sitter_vue::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Kotlin => tree_sitter_kotlin_sg::LANGUAGE.into(),
            Self::Swift => tree_sitter_swift::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Self::Lua => tree_sitter_lua::LANGUAGE.into(),
            Self::Perl => tree_sitter_perl::LANGUAGE.into(),
            Self::Pascal => tree_sitter_pascal::LANGUAGE.into(),
            Self::R => tree_sitter_r::LANGUAGE.into(),
            Self::Groovy => dekobon_tree_sitter_groovy::LANGUAGE.into(),
            Self::ObjC => tree_sitter_objc::LANGUAGE.into(),
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
