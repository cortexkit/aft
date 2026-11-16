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

    pub fn compile_pattern(&self, pattern: &str) -> Result<Pattern, PatternError> {
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

    /// Candidate file extensions. Ambiguous `.h` files are resolved by `matches_path`.
    pub fn extensions(&self) -> &'static [&'static str] {
        match self {
            Self::TypeScript => &["ts", "mts", "cts"],
            Self::Tsx => &["tsx"],
            Self::JavaScript => &["js", "mjs", "cjs", "jsx"],
            Self::Python => &["py", "pyi"],
            Self::Rust => &["rs"],
            Self::Go => &["go"],
            Self::C => &["c", "h"],
            Self::Cpp => &["cc", "cpp", "cxx", "hpp", "hh", "h"],
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
            Self::ObjC => &["m", "mm", "h"],
        }
    }

    /// Check if a file extension matches this language.
    pub fn matches_extension(&self, ext: &str) -> bool {
        self.extensions().contains(&ext)
    }

    /// Check a path's language, resolving ambiguous headers from their contents.
    pub fn matches_path(&self, path: &std::path::Path) -> bool {
        if path.extension().and_then(|ext| ext.to_str()) == Some("h") {
            return matches!(
                (self, crate::parser::detect_language(path)),
                (Self::C, Some(crate::parser::LangId::C))
                    | (Self::Cpp, Some(crate::parser::LangId::Cpp))
                    | (Self::ObjC, Some(crate::parser::LangId::ObjC))
            );
        }
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

test_support_visibility! {
fn pascal_pattern_is_declaration(pattern: &str) -> bool {
    let trimmed = pattern.trim_start().to_lowercase();
    [
        "program",
        "unit",
        "procedure",
        "function",
        "constructor",
        "destructor",
        "type",
        "const",
        "var",
    ]
    .iter()
    .any(|keyword| {
        trimmed.strip_prefix(keyword).is_some_and(|rest| {
            rest.chars()
                .next()
                .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
        })
    })
}
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
