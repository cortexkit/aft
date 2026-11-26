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
    Nix,
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
            "nix" => Some(Self::Nix),
            _ => None,
        }
    }

    pub fn compile_pattern(&self, pattern: &str) -> Result<Pattern, PatternError> {
        if matches!(self, Self::Nix) {
            // A bare binding (`$NAME = true;`) or `inherit` clause is not a
            // complete Nix expression, so a whole-expression pattern fails to
            // parse. Match the member inside an attrset context instead: the
            // selector scopes the pattern to that member's own kind, which
            // keeps sibling bindings intact on replace.
            return Pattern::try_new(pattern, self.clone()).or_else(|_| {
                // Comments carry no match semantics; strip them (outside
                // string contents) before building the attrset context, so
                // `$A = true; # note` matches like `$A = true;`.
                let stripped = nix_pattern_without_comments(pattern);
                let context = format!("{{ {stripped} }}");
                match nix_wrapper_selector(&context) {
                    Some(selector) => Pattern::contextual(&context, selector, self.clone()),
                    // Several members would each match on their own, silently
                    // widening or narrowing what the user asked for.
                    None => Err(PatternError::Parse(pattern.to_string())),
                }
            });
        }

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
            Self::Nix => &["nix"],
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

/// Drop `#` line comments and `/* … */` block comments from a Nix pattern
/// outside string contents, so a comment does not change what the pattern
/// matches. A dropped block comment leaves one space: comments separate
/// tokens, they never fuse them. The string model matches the hint scanner's:
/// `"` with backslash escapes, `''` indented strings where `'''`, `''$` and
/// `''\x` stay inside; `${ … }` interpolation bodies are full expressions, so
/// strings may nest inside them and only a frame stack exits correctly.
fn nix_pattern_without_comments(pattern: &str) -> String {
    let bytes = pattern.as_bytes();
    let states = crate::ast_grep_hints::nix_lex_code_states(bytes);
    let mut guard = crate::ast_grep_hints::NixApostropheGuard::new(bytes);
    let mut out = String::with_capacity(pattern.len());
    let mut i = 0;
    let mut strings: Vec<crate::ast_grep_hints::NixStrFrame> = Vec::new();
    while i < bytes.len() {
        if !strings.is_empty() {
            // Copy the string region verbatim until top level is reached.
            // Regions are copied whole, not per span: a single span may end
            // mid-glyph (escapes), while a region always starts after an
            // opening quote and ends after a closing quote or at EOF.
            let start = i;
            while i < bytes.len() && !strings.is_empty() {
                i += crate::ast_grep_hints::nix_string_step(
                    bytes,
                    i,
                    &mut strings,
                    &states,
                    &mut guard,
                );
            }
            out.push_str(&pattern[start..i]);
            continue;
        }
        let b = bytes[i];
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            // Block comments are content-free for matching too: drop them
            // whole (a `#` inside one must not trigger line-comment stripping
            // and leave the block unterminated).
            crate::ast_grep_hints::nix_skip_block_comment(bytes, &mut i);
            // Pad the hole with whitespace: dropping a comment between two
            // tokens must not fuse them (`f/* c */x` is an application, `fx`
            // is one name).
            out.push(' ');
            continue;
        }
        if b == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
            if guard.continues(bytes, i, &states) {
                // Apostrophes inside identifiers (`foo''`) are content;
                // opening a string region here would copy the trailing
                // comment verbatim and hide the wrapper's closing brace.
                out.push('\'');
                i += 1;
                continue;
            }
            out.push_str("''");
            strings.push(crate::ast_grep_hints::NixStrFrame {
                indented: true,
                braces: 0,
            });
            i += 2;
            continue;
        }
        if b == b'"' {
            out.push('"');
            strings.push(crate::ast_grep_hints::NixStrFrame {
                indented: false,
                braces: 0,
            });
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len()
            && !matches!(bytes[i], b'#' | b'"')
            && !(bytes[i] == b'\'' && bytes.get(i + 1) == Some(&b'\''))
            && !(bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*'))
        {
            i += 1;
        }
        out.push_str(&pattern[start..i]);
    }
    out
}
/// Replace ast-grep metavariables with an ordinary identifier so the grammar
/// can parse the pattern without its metavariable sentinels.
fn nix_pattern_without_meta_variables(pattern: &str) -> String {
    let mut plain = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '$' {
            plain.push(character);
            continue;
        }
        let mut signature = String::new();
        while chars.peek() == Some(&'$') {
            signature.push(chars.next().expect("peeked"));
        }
        if chars.peek() == Some(&'{') {
            // `$` is never a metavariable when `{` follows: the lexer treats
            // `$$` as a literal-dollar pair and the last `$` opens a `${ … }`
            // interpolation, which is exactly what `f = "$$${expr}";` needs.
            // Collapsing the run here deleted the interpolation opener.
            plain.push('$');
            plain.push_str(&signature);
            continue;
        }
        let mut name = String::new();
        while let Some(next) = chars.peek() {
            // `'` is deliberately excluded: it can both continue a Nix
            // identifier and close an indented-string delimiter (`''`), and
            // greediness here ate the closing delimiter of `''echo $out''`,
            // rejecting a valid pattern. Metavariable names never contain
            // `'`, so stopping early keeps string bodies intact.
            if next.is_alphanumeric() || *next == '_' {
                name.push(chars.next().expect("peeked"));
            } else {
                break;
            }
        }
        if signature.is_empty() && name.is_empty() {
            plain.push('$');
        } else {
            plain.push_str("_aft_meta");
        }
    }
    plain
}

/// Rewrite `$`-dialect metavariables into Nix's `_` expando only where the
/// lexer reads code. Inside strings and comments a `$` is literal content,
/// so the run arithmetic mirrors the repo's own generic converter
/// (`pre_process_pattern_with_expando` below) and ast-grep-core's
/// `extract_meta_var` (meta_var.rs) byte for byte: a `$` or `$$` run before
/// an upper-case/`_` name and a bare `$$$` ellipsis become the same number
/// of `_`; anything else stays verbatim. The code/string split comes from
/// `nix_lex_code_states`, which follows scanner.c's `$`-swallow rule — only
/// an odd dollar-run before `{` opens an interpolation.
fn nix_pre_process_pattern(query: &str) -> String {
    let bytes = query.as_bytes();
    let states = crate::ast_grep_hints::nix_lex_code_states(bytes);
    let mut out = String::with_capacity(query.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && states[i] {
            let mut run = 0;
            while i + run < bytes.len() && bytes[i + run] == b'$' {
                run += 1;
            }
            let next = bytes.get(i + run).copied();
            let need_replace =
                matches!(next, Some(c) if c.is_ascii_uppercase() || c == b'_') || run == 3;
            for _ in 0..run {
                out.push(if need_replace { '_' } else { '$' });
            }
            i += run;
            continue;
        }
        // Push one char at a time: the source is UTF-8 and only `$` bytes
        // are ever replaced, all of them ASCII.
        let ch_len = 1 + match bytes[i] {
            b if b >= 0xF0 => 3,
            b if b >= 0xE0 => 2,
            b if b >= 0xC0 => 1,
            _ => 0,
        };
        out.push_str(&query[i..(i + ch_len).min(query.len())]);
        i += ch_len;
    }
    out
}

/// The selector a contextual Nix pattern must use: the sole member's own node
/// kind. `inherit`/`inherit_from` clauses are real syntax worth finding, so
/// they are addressable on their own; a wrapper holding any other number of
/// members is rejected because the selector would match one of them and
/// silently narrow or widen the query.
/// Node-kind names here couple to the tree-sitter-nix version, as in the
/// parser.rs Nix block head.
fn nix_wrapper_selector(source: &str) -> Option<&'static str> {
    let plain = nix_pattern_without_meta_variables(source);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_nix::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(&plain, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let expression = root.child_by_field_name("expression")?;
    let mut cursor = expression.walk();
    let set = expression
        .named_children(&mut cursor)
        .find(|child| child.kind() == "binding_set")?;
    let mut set_cursor = set.walk();
    // Comments carry no members: `$A = true; # note` is a single-binding
    // pattern, exactly as in the language itself.
    let members: Vec<tree_sitter::Node> = set
        .named_children(&mut set_cursor)
        .filter(|child| child.kind() != "comment")
        .collect();
    let [only] = members.as_slice() else {
        return None;
    };
    match only.kind() {
        "binding" => Some("binding"),
        "inherit" => Some("inherit"),
        "inherit_from" => Some("inherit_from"),
        _ => None,
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
        if matches!(self, Self::Nix) {
            // The generic converter is text-blind: it rewrites every `$$$`
            // run — including inside string contents, where `$$` is literal
            // text and the last `$` opens a `${ … }` interpolation — so
            // patterns like `script = "$$${f "#" xs}";` were corrupted
            // before parsing. Nix converts `$` sigils only where the lexer
            // reads code; string and comment bytes pass through verbatim.
            return Cow::Owned(nix_pre_process_pattern(query));
        }
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
            Self::Pascal | Self::Nix => '_',
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
            Self::Nix => tree_sitter_nix::LANGUAGE.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ast_grep_core::tree_sitter::LanguageExt;

    #[test]
    fn nix_binding_search_tolerates_quotes_nested_in_interpolation() {
        use ast_grep_core::tree_sitter::LanguageExt;
        let lang = AstGrepLang::Nix;
        for pattern in [
            r##"$A = "${builtins.concatStringsSep "#" xs}";"##,
            "$A = ''${ builtins.concatStringsSep \"#\" xs }'';",
            r#"$A = "x ${ "in;ner #y" } z";"#,
            "$A = ''hi ${ \"don''t; do\" } x'';",
            "$A = ''a''\\$b'';",
            r#"$A = "${ /* " */ toString 1 }"; # note"#,
            r"foo'' = true; # note",
            "$A = ''${foo''}''; # note",
            r"foo-'' = true; # note",
            r"f = 1''a#b'';",
            r"x = ./d''a#b'';",
            "a.foo'' = 1; # note",
            "f = a -b''; # note",
            "f = 9x''; # note",
            "f = a+foo''; # note",
            "f = 1.5e2''a#b''; # note",
            "f = /* c */b''; # note",
            "f = ''${/* c */x''}''; # note",
            "f = \"/*\" + x*/y'' # c '';",
            "f = .5e2''a#b''; # note",
            "f = 1.5e2-3''a#b''; # note",
            "f = 1.5e2-b''; # note",
            "f = 0.e2''; # note",
            "f = 9x''z/y'' # note '';",
            "f = a\"${x''y}\" /b'' # note '';",
            "f = ''${ ./d''a#b'' }''; # note",
            "f = ''a'\\'' /b'' # note '';",
        ] {
            let compiled = lang.compile_pattern(pattern);
            assert!(compiled.is_ok(), "{pattern} must compile: {compiled:?}");
        }
        let pattern = lang
            .compile_pattern(r##"$A = "${builtins.concatStringsSep "#" xs}";"##)
            .expect("nested quotes compile");
        let grep = lang.ast_grep(r##"{ a = "${builtins.concatStringsSep "#" xs}"; b = 1; }"##);
        let matched: Vec<String> = grep
            .root()
            .find_all(&pattern)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(
            matched,
            vec![r##"a = "${builtins.concatStringsSep "#" xs}";"##]
        );
    }

    #[test]
    fn nix_comment_removal_cannot_fuse_tokens() {
        use ast_grep_core::tree_sitter::LanguageExt;
        let lang = AstGrepLang::Nix;
        let pattern = lang
            .compile_pattern("$A = f/* c */x;")
            .expect("a comment between two tokens must still compile");
        // The comment separates an application, not one fused name.
        let grep = lang.ast_grep("{ a = f x; }");
        let matched: Vec<String> = grep
            .root()
            .find_all(&pattern)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(matched, vec!["a = f x;"]);
        let grep = lang.ast_grep("{ a = fx; }");
        let fused: Vec<String> = grep
            .root()
            .find_all(&pattern)
            .map(|n| n.text().to_string())
            .collect();
        assert!(fused.is_empty(), "a comment must not turn `f x` into `fx`");
    }

    #[test]
    fn test_nix_contextual_pattern_tolerates_comments_outside_strings() {
        use ast_grep_core::tree_sitter::LanguageExt;
        let lang = AstGrepLang::Nix;
        let pattern = lang
            .compile_pattern("$A = true; # note")
            .expect("a trailing comment must not defeat the contextual fallback");
        let grep = lang.ast_grep("{ a = true; b = false; }");
        let matched: Vec<String> = grep
            .root()
            .find_all(&pattern)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(matched, vec!["a = true;"]);

        // Block comments are dropped whole: a `#` inside one must not be
        // treated as a line comment (which would strand the `/*`).
        let pattern = lang
            .compile_pattern("$A = /* # note */ true;")
            .expect("a block comment must not defeat the contextual fallback");
        let grep = lang.ast_grep("{ a = true; b = false; }");
        let matched: Vec<String> = grep
            .root()
            .find_all(&pattern)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(matched, vec!["a = true;"]);
        let multiline = lang
            .compile_pattern("$A = /* a\n# b\n*/ true;")
            .expect("a multiline block comment is dropped whole");
        let grep = lang.ast_grep("{ a = true; b = false; }");
        let matched: Vec<String> = grep
            .root()
            .find_all(&multiline)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(matched, vec!["a = true;"]);

        // A `#` inside string contents survives stripping: the pattern still
        // describes the quoted attribute exactly.
        let pattern = lang
            .compile_pattern("$A = \"x#y\";")
            .expect("a `#` inside a string is pattern content, not a comment");
        let grep = lang.ast_grep("{ a = \"x#y\"; b = 2; }");
        let matched: Vec<String> = grep
            .root()
            .find_all(&pattern)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(matched, vec!["a = \"x#y\";"]);
    }

    #[test]
    fn test_nix_contextual_nested_value_binds_outer_binding() {
        use ast_grep_core::tree_sitter::LanguageExt;
        let lang = AstGrepLang::Nix;
        // The selector gate counts direct members only, so the inner `b = 1`
        // is invisible to it; the contextual fallback must still resolve the
        // pattern to the OUTER binding, not silently match the nested one.
        let source = "{ outer = { b = 1; }; inner = 2; }";
        let pattern = AstGrepLang::Nix
            .compile_pattern("$A = { b = 1; };")
            .expect("attrset-valued single binding compiles via contextual fallback");
        let grep = lang.ast_grep(source);
        let matched: Vec<String> = grep
            .root()
            .find_all(&pattern)
            .map(|node| node.text().to_string())
            .collect();
        assert_eq!(
            matched.len(),
            1,
            "exactly the outer binding matches: {matched:?}"
        );
        assert!(
            matched[0].starts_with("outer"),
            "the match must be the outer binding, not the nested one: {:?}",
            matched[0]
        );
    }

    #[test]
    fn test_nix_language_and_contextual_binding_patterns() {
        let lang = AstGrepLang::Nix;
        assert_eq!(AstGrepLang::from_str("nix"), Some(AstGrepLang::Nix));
        assert!(lang.extensions().contains(&"nix"));

        // A bare binding is not a complete Nix expression; the contextual
        // fallback must scope the pattern to one binding.
        let source = "{ foo = true; bar = false; baz.qux = true; }";
        let pattern = lang
            .compile_pattern("$NAME = true;")
            .expect("contextual binding pattern compiles");
        let grep = lang.ast_grep(source);
        let root = grep.root();
        let matched: Vec<String> = root
            .find_all(&pattern)
            .map(|node| node.text().to_string())
            .collect();
        assert_eq!(matched, vec!["foo = true;", "baz.qux = true;"]);

        // Replacing must touch only the matched bindings, never siblings.
        let mut edits = root.replace_all(&pattern, "$NAME = false;");
        edits.sort_by(|a, b| b.position.cmp(&a.position));
        let mut bytes = source.as_bytes().to_vec();
        for edit in edits {
            bytes.splice(
                edit.position..edit.position + edit.deleted_length,
                edit.inserted_text.iter().copied(),
            );
        }
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "{ foo = false; bar = false; baz.qux = false; }"
        );

        // Several bindings in one pattern must not degrade into per-binding
        // matches that silently drop the adjacency the user asked for.
        assert!(
            lang.compile_pattern("$A = true; $B = false;").is_err(),
            "multi-binding patterns are rejected"
        );
        assert!(
            lang.compile_pattern("$A = true; inherit $B;").is_err(),
            "a wrapper with an inherit member is rejected"
        );

        // The `_` sentinel only rewrites `$`-metavariables; a literal identifier
        // written with a leading underscore stays literal and matches itself.
        let underscore = lang
            .compile_pattern("_module.args = $V;")
            .expect("underscore pattern compiles");
        let literal = lang
            .ast_grep("{ _module.args = 1; other.args = 2; }")
            .root()
            .find_all(&underscore)
            .count();
        assert_eq!(literal, 1, "a literal `_`-prefixed key matches itself only");
        let unrelated = lang
            .ast_grep("{ other.args = 1; }")
            .root()
            .find_all(&underscore)
            .count();
        assert_eq!(unrelated, 0, "the literal key does not widen the match");
        // `_` followed by an uppercase letter is still a metavariable.
        let upper = lang
            .compile_pattern("_FOO = $V;")
            .expect("uppercase pattern compiles");
        let upper_matches = lang
            .ast_grep("{ _FOO = 1; bar = 2; }")
            .root()
            .find_all(&upper)
            .count();
        assert_eq!(upper_matches, 2, "`_` + uppercase binds as a metavariable");
    }

    #[test]
    fn test_nix_contextual_inherit_patterns() {
        let lang = AstGrepLang::Nix;
        let source = "{ foo = true; inherit bar; inherit (lib) mkIf; }";

        let plain = lang
            .compile_pattern("inherit $NAME;")
            .expect("an inherit clause pattern compiles");
        let matched: Vec<String> = lang
            .ast_grep(source)
            .root()
            .find_all(&plain)
            .map(|node| node.text().to_string())
            .collect();
        assert_eq!(matched, vec!["inherit bar;"]);

        let from = lang
            .compile_pattern("inherit ($SRC) $NAME;")
            .expect("an inherit-from pattern compiles");
        let matched: Vec<String> = lang
            .ast_grep(source)
            .root()
            .find_all(&from)
            .map(|node| node.text().to_string())
            .collect();
        assert_eq!(matched, vec!["inherit (lib) mkIf;"]);
    }

    #[test]
    fn test_nix_indented_string_dollar_is_not_a_metavariable_hungry_scan() {
        let lang = AstGrepLang::Nix;
        // `$out` sits inside an indented string; the metavariable scan must
        // stop at the name and leave the closing `''` in place, or the
        // wrapper parse sees an unfinished string and the pattern is
        // rejected.
        lang.compile_pattern("script = ''echo $out'';")
            .expect("indented-string interpolation pattern compiles");
        lang.compile_pattern("a = \"$x\";")
            .expect("quoted-string metavariable pattern compiles");
        let pattern = lang
            .compile_pattern("$NAME = $VAL;")
            .expect("generic binding pattern compiles");
        let matched: Vec<String> = lang
            .ast_grep("{ script = ''echo $out''; other = 1; }")
            .root()
            .find_all(&pattern)
            .map(|node| node.text().to_string())
            .collect();
        assert_eq!(matched, vec!["script = ''echo $out'';", "other = 1;"]);
    }

    #[test]
    fn nix_escaped_dollar_run_keeps_the_interpolation_open() {
        // Greptile P2: in `f = "$$${expr}";` the `$$` is literal text and the
        // last `$` opens the interpolation. Collapsing the whole `$` run into
        // the metavariable stand-in deleted that opener, so the inner quote
        // closed the outer string and the `#` hid the rest — valid binding
        // searches were rejected.
        use ast_grep_core::tree_sitter::LanguageExt;
        let lang = AstGrepLang::Nix;
        for pattern in [
            r##"script = "$$${builtins.concatStringsSep "#" xs}";"##,
            r##"f = "$$${builtins.trace "x" y}";"##,
        ] {
            let compiled = lang.compile_pattern(pattern);
            assert!(compiled.is_ok(), "{pattern} must compile: {compiled:?}");
        }
        let pattern = lang
            .compile_pattern(r##"script = "$$${a}";"##)
            .expect("escaped dollar run compiles");
        let grep = lang.ast_grep(r##"{ script = "$$${a}"; other = 1; }"##);
        let matched: Vec<String> = grep
            .root()
            .find_all(&pattern)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(matched, vec![r##"script = "$$${a}";"##]);
        // The stand-in is still a plain literal: a target with the stand-in's
        // replacement text must not be matched by the escaped-dollar pattern.
        let grep = lang.ast_grep(r##"{ script = "_aft_meta{a}"; }"##);
        let wrong: Vec<String> = grep
            .root()
            .find_all(&pattern)
            .map(|n| n.text().to_string())
            .collect();
        assert!(wrong.is_empty(), "literal text must not leak into matching");
    }

    #[test]
    fn nix_many_commented_identifier_elements_compile_in_linear_time() {
        // Greptile P1: repeated `/* c */x''` elements re-entered the
        // apostrophe guard for every earlier element, so compile time
        // doubled per element (measured: k=24 took ~1.6 s, k=32 tens of
        // seconds). The guard now reads a single forward lex-state table;
        // this budget has ~4 orders of margin over the fixed behavior and
        // fails the recursive design by minutes at k=32.
        let mut pattern = String::from("f = [");
        for n in 0..32 {
            pattern.push_str(&format!(" /* c */x{n}''"));
        }
        pattern.push_str(" ]; # tail");
        let start = std::time::Instant::now();
        let compiled = AstGrepLang::Nix.compile_pattern(&pattern);
        let elapsed = start.elapsed();
        assert!(compiled.is_ok(), "must compile: {compiled:?}");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "compile took {elapsed:?}"
        );
    }

    #[test]
    fn nix_even_dollar_runs_stay_string_literal() {
        // scanner.c: `$` swallows the following char unless it stops there,
        // so only an ODD dollar-run before `{` interpolates — `$${` is two
        // literal dollars plus a brace, and the bytes after it stay string
        // content. An even run must never open a phantom interpolation body
        // (which would classify later bytes as code and let the expando
        // pre-process rewrite string contents).
        use ast_grep_core::Language as _;
        for pattern in [
            r##"f = "$${$A}";"##,
            r##"f = ''$${$B}'';"##,
            // `$` stops before a quote pair that the scanner must still see
            // as delimiter: indented closes, simple swallows the lone `'`.
            r##"f = ''$'';"##,
            r##"f = "$'q';"##,
        ] {
            let processed = AstGrepLang::Nix.pre_process_pattern(pattern);
            assert_eq!(processed.as_ref(), pattern, "even dollar run mangled");
        }
    }

    #[test]
    fn nix_dollar_literal_string_still_exposes_code_metavariables() {
        // The phantom-body bug classified every byte after `$${''` as string
        // content (the stray indented frame never closed), so `$B` in code
        // lost its expando conversion and the wrapped context parsed with an
        // ERROR node. The pattern must compile AND bind both metavars.
        let pattern = r##"{ b = "$${''x}"; c = $B; }"##;
        let compiled = AstGrepLang::Nix.compile_pattern(pattern);
        assert!(compiled.is_ok(), "must compile: {compiled:?}");
        use ast_grep_core::tree_sitter::LanguageExt;
        let lang = AstGrepLang::Nix;
        let compiled = compiled.expect("pattern compiles");
        let grep = lang.ast_grep(r##"{ b = "$${''x}"; c = r; }"##);
        let hits: Vec<String> = grep
            .root()
            .find_all(&compiled)
            .map(|n| n.text().to_string())
            .collect();
        assert_eq!(hits, vec![r##"{ b = "$${''x}"; c = r; }"##]);
    }
}
