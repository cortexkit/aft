//! Pattern hint detection for ast-grep search/replace.
//!
//! When agents send patterns that LOOK plausible but cannot match (regex
//! escapes, character classes, alternations, language-specific shape mistakes),
//! we attach a `hint` field to the response so the agent can correct course
//! instead of silently believing "0 matches" means "no work to do".
//!
//! The two big agent-facing failure modes this addresses:
//!
//! 1. **Regex thinking.** Agents trained on grep send `\w+`, `[a-z]`, `foo.*bar`,
//!    `foo|bar` — these compile cleanly as ast-grep patterns but never match.
//!
//! 2. **Language-specific shape gotchas.** `def $FN($$$):`  with trailing colon
//!    in Python, `function $NAME` with no body in TS, `fn $X` with no body in
//!    Rust — these all parse but fail to match real source.
//!
//! Modeled after oh-my-openagent's `pattern-hints.ts`, with the Rust match-arm
//! `|` case added (the bug that hit me on 2026-05-06 while doing the Solidity
//! integration).

use crate::ast_grep_lang::AstGrepLang;

/// Returns a human-readable hint when a pattern looks like a common mistake,
/// `None` otherwise. Order matters: regex-shaped patterns are flagged before
/// language-specific shape mistakes, because regex syntax dominates the
/// failure-mode signal.
pub fn detect_pattern_hint(pattern: &str, lang: &AstGrepLang) -> Option<String> {
    detect_regex_misuse(pattern).or_else(|| detect_language_specific_mistake(pattern, lang))
}

/// Detect regex-flavored patterns that don't apply to ast-grep.
///
/// ast-grep matches AST nodes, not text. The wildcards and alternations from
/// grep/ripgrep don't carry over.
pub fn detect_regex_misuse(pattern: &str) -> Option<String> {
    let src = pattern.trim();

    // \w \W \d \D \s \S \b — regex escapes that look like backslash sequences.
    // Tree-sitter source rarely contains these literally, so they're a strong
    // signal the agent meant a regex.
    if has_regex_escape(src) {
        return Some(
            "Hint: \"\\w\", \"\\d\", \"\\s\", \"\\b\" are regex escapes. ast-grep matches AST nodes, \
             not text — use $VAR for any identifier, $$$ for multiple nodes, or switch to grep \
             for text search."
                .to_string(),
        );
    }

    // [a-z] / [0-9] — regex character classes. ast-grep has no equivalent.
    // We're careful NOT to flag `arr[0]` or `map[key]` (legitimate AST shapes).
    if has_char_class_range(src) {
        return Some(
            "Hint: \"[a-z]\" / \"[0-9]\" character classes are regex, not AST. Use $VAR to match \
             any identifier, or switch to grep for text search."
                .to_string(),
        );
    }

    // foo.*bar / id.+suffix — regex wildcards embedded in identifiers (no `$`
    // meta-vars at all). `.*` means "any chars" in regex but is parsed as
    // member-access in most ast-grep grammars and almost never matches.
    if !src.contains('$') && has_regex_wildcard_in_identifier(src) {
        return Some(
            "Hint: \".*\" / \".+\" are regex wildcards. In ast-grep use $$$ for multiple AST nodes \
             and $VAR for a single node. For text patterns, switch to grep."
                .to_string(),
        );
    }

    // foo|bar / Foo::A | Foo::B / func.*build|Build — alternation.
    // ast-grep parses `|` according to the target language grammar (bitwise-or
    // in C/Rust/etc., closure delimiter in Rust, alternation in regex). It
    // never matches "any of these alternatives" the way grep -E does.
    if looks_like_alternation(src) {
        return Some(
            "Hint: \"|\" does NOT mean alternation in ast-grep — patterns are AST-shaped, not \
             text-shaped. ast-grep parses `|` as bitwise-or / Rust match-arm / closure delimiter \
             (depending on language), not as \"match any of these\". Options: (a) fire one \
             ast_grep_search per alternative, (b) wrap in an enclosing AST node that captures the \
             whole match-arm or expression, or (c) use grep with a regex like \"foo|bar\"."
                .to_string(),
        );
    }

    None
}

/// Detect language-specific shape mistakes — patterns that are syntactically
/// "almost right" but don't form a complete AST node for the target language.
pub fn detect_language_specific_mistake(pattern: &str, lang: &AstGrepLang) -> Option<String> {
    let src = pattern.trim();

    match lang {
        AstGrepLang::Python => {
            // `def foo($$$):` and `class C:` — trailing colon without a body.
            // Python's grammar requires a body; ast-grep's pattern parser
            // refuses to bind to a function/class node without one.
            if (src.starts_with("def ") || src.starts_with("async def ")) && src.ends_with(':') {
                return Some(format!(
                    "Hint: Python def patterns need a body. Try: \"{}\\n    $$$\" \
                     (or drop the trailing colon and add `: $$$`).",
                    &src[..src.len() - 1]
                ));
            }
            if src.starts_with("class ") && src.ends_with(':') {
                return Some(format!(
                    "Hint: Python class patterns need a body. Try: \"{}\\n    $$$\".",
                    &src[..src.len() - 1]
                ));
            }
        }
        AstGrepLang::TypeScript | AstGrepLang::Tsx | AstGrepLang::JavaScript => {
            // `function $NAME` / `export async function $NAME` — name only,
            // no params/body. Function declarations are matched as full nodes.
            if regex_function_name_only_js(src) {
                return Some(
                    "Hint: JS/TS function patterns need params and body. Try: \
                     \"function $NAME($$$) { $$$ }\" (or `export async function ...`)."
                        .to_string(),
                );
            }
        }
        AstGrepLang::Go => {
            if regex_func_name_only_go(src) {
                return Some(
                    "Hint: Go function patterns need params and body. Try: \
                     \"func $NAME($$$) { $$$ }\" (or `func ($R Recv) $NAME($$$) ...`)."
                        .to_string(),
                );
            }
        }
        AstGrepLang::Rust => {
            if regex_fn_name_only_rust(src) {
                return Some(
                    "Hint: Rust fn patterns need params and body. Try: \
                     \"fn $NAME($$$) { $$$ }\" (or `pub fn ...` / `async fn ...`)."
                        .to_string(),
                );
            }
        }
        _ => {}
    }

    None
}

// ---------------------------------------------------------------------------
// Internal predicates
// ---------------------------------------------------------------------------

/// `\w \W \d \D \s \S \b \B` — the regex character-class escapes. We look at
/// the raw bytes to avoid pulling in a regex-engine dependency just for this.
fn has_regex_escape(src: &str) -> bool {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\\' {
            let next = bytes[i + 1];
            if matches!(next, b'w' | b'W' | b'd' | b'D' | b's' | b'S' | b'b' | b'B') {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// `[a-z]` / `[0-9]` / `[A-Za-z0-9]` — character class with at least one
/// `lo-hi` range. We deliberately accept `arr[0]`, `map["key"]`, and
/// meta-var indexing because none of those have a `-` between alphanumerics
/// inside `[...]`.
fn has_char_class_range(src: &str) -> bool {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            // Scan up to the matching `]`. If we find `X-Y` where both X and Y
            // are alphanumeric ASCII, that's a regex character-class range.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b']' {
                if j + 2 < bytes.len()
                    && bytes[j + 1] == b'-'
                    && is_ascii_alnum(bytes[j])
                    && is_ascii_alnum(bytes[j + 2])
                {
                    return true;
                }
                j += 1;
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    false
}

/// `foo.*bar` / `id.+suffix` — a `.` followed by `*` or `+` immediately after
/// a word character. We only flag this when the pattern has no `$` at all
/// (otherwise the agent is already thinking in meta-vars and `.*` might be
/// inside a regex matcher param or a comment).
fn has_regex_wildcard_in_identifier(src: &str) -> bool {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if is_ascii_word(bytes[i])
            && bytes[i + 1] == b'.'
            && (bytes[i + 2] == b'*' || bytes[i + 2] == b'+')
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Heuristic alternation detector. Three shapes we want to catch:
///
/// 1. Pure text alternation: `foo|bar`, `noEmit|NoEmit|emit`,
///    `watch|--watch|WatchMode`. (oh-my-openagent's regex.)
///
/// 2. Path-segment alternation: `Foo::A | Foo::B | Foo::C` — Rust match-arm
///    "alternative" written outside an enclosing match expression. ast-grep
///    parses `|` as bitwise-or here; the pattern compiles but matches zero.
///    This is the bug that hit me today.
///
/// 3. Module-path alternation: `path::To::A | path::To::B`.
///
/// We deliberately accept legitimate `|` shapes:
///
/// - `$A | $B` — meta-var bitwise/match alternative. The agent has already
///   moved into AST thinking.
/// - `|x| x + 1` — Rust closure. Starts with `|`, not surrounded by
///   identifier text.
/// - Any `|` inside a string literal `"..."` or `'...'`.
fn looks_like_alternation(src: &str) -> bool {
    if !src.contains('|') {
        return false;
    }

    // Skip Rust closures: `|args| body`. A closure starts with `|`, not with an
    // identifier-ish token. We're defensive — if the pattern starts with `|`
    // (after trimming), we treat it as a closure / unrelated.
    if src.starts_with('|') {
        return false;
    }

    // Strip string literals so a `|` inside `"foo|bar"` doesn't trip us.
    let stripped = strip_string_literals(src);

    // No `|` left after stripping strings → it was all inside strings.
    if !stripped.contains('|') {
        return false;
    }

    // `$A | $B` and `$X.foo | $Y.bar` — already meta-var-shaped. Let it pass.
    // Rule of thumb: if every `|` in the stripped pattern is adjacent (with
    // optional whitespace) to a `$`, the agent is in AST mode.
    if all_pipes_neighbor_meta_var(&stripped) {
        return false;
    }

    // Now look for the actual alternation shape: identifier-or-path-or-call
    // text on both sides of every top-level `|`. We split on `|` (outside
    // strings, already stripped above), trim each part, and require that each
    // non-empty part looks like a code-token: identifier, `::`-path,
    // `.`-path, or call.
    let parts: Vec<&str> = stripped.split('|').map(|p| p.trim()).collect();
    if parts.len() < 2 {
        return false;
    }

    parts
        .iter()
        .all(|p| !p.is_empty() && looks_like_code_token(p))
}

/// `Foo::Bar`, `mod::Foo`, `obj.field`, `func()`, `Identifier`, `123`.
fn looks_like_code_token(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Strip a trailing call `(...)` or index `[...]` so `foo()` and `arr[0]`
    // both reduce to identifier-ish text.
    let trimmed = s.trim_end_matches(|c: char| c == ')' || c == ']');
    let trimmed = trimmed.trim_end_matches(|c: char| c.is_ascii_digit());

    trimmed.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '(' | '[' | '"' | '\'' | '-')
    })
}

/// True when every `|` in the (string-stripped) input has a `$` somewhere on
/// the same logical "side" — meaning the agent is using `|` as bitwise-or or
/// match-alternative around meta-vars, not as text alternation.
///
/// Cheap heuristic: split on `|`, check that adjacent parts contain at least
/// one `$`. This favors false negatives over false positives — we'd rather
/// miss a hint than emit a wrong one.
fn all_pipes_neighbor_meta_var(src: &str) -> bool {
    let parts: Vec<&str> = src.split('|').collect();
    if parts.len() < 2 {
        return false;
    }
    parts.iter().all(|p| p.contains('$'))
}

/// Strip `"..."` and `'...'` content, replacing the inside with spaces so
/// indices stay aligned. Honors `\"` / `\'` escapes.
fn strip_string_literals(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' {
            out.push(c);
            let quote = c;
            while let Some(&inner) = chars.peek() {
                chars.next();
                if inner == '\\' {
                    out.push(' ');
                    if let Some(&_escaped) = chars.peek() {
                        chars.next();
                        out.push(' ');
                    }
                    continue;
                }
                if inner == quote {
                    out.push(inner);
                    break;
                }
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_ascii_word(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

fn is_ascii_alnum(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9')
}

// Language-specific shape regexes hand-rolled (no regex crate dep needed).

fn regex_function_name_only_js(src: &str) -> bool {
    // Matches: `function $NAME`, `export function $NAME`, `async function $NAME`,
    // `export async function $NAME`. No `(` after the name.
    let mut rest = src;
    if let Some(after) = rest.strip_prefix("export ") {
        rest = after.trim_start();
    }
    if let Some(after) = rest.strip_prefix("async ") {
        rest = after.trim_start();
    }
    let after_fn = match rest.strip_prefix("function ") {
        Some(after) => after.trim_start(),
        None => return false,
    };
    // Must be `$NAME` (meta-var) and nothing else after it.
    name_only_meta_var(after_fn)
}

fn regex_func_name_only_go(src: &str) -> bool {
    let after_fn = match src.strip_prefix("func ") {
        Some(after) => after.trim_start(),
        None => return false,
    };
    name_only_meta_var(after_fn)
}

fn regex_fn_name_only_rust(src: &str) -> bool {
    let mut rest = src;
    if let Some(after) = rest.strip_prefix("pub ") {
        rest = after.trim_start();
    }
    if let Some(after) = rest.strip_prefix("async ") {
        rest = after.trim_start();
    }
    let after_fn = match rest.strip_prefix("fn ") {
        Some(after) => after.trim_start(),
        None => return false,
    };
    name_only_meta_var(after_fn)
}

/// True when `s` is exactly `$NAME` (meta-var) plus optional trailing
/// whitespace, with NO `(` or `{` after — i.e. a name-only function pattern.
fn name_only_meta_var(s: &str) -> bool {
    let trimmed = s.trim_end();
    if !trimmed.starts_with('$') {
        return false;
    }
    let name_chars = trimmed.chars().skip(1);
    let mut saw_any = false;
    for c in name_chars {
        if c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit() {
            saw_any = true;
            continue;
        }
        return false;
    }
    saw_any
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- regex misuse: backslash escapes ---------------------------------

    #[test]
    fn flags_backslash_w_escape() {
        let hint = detect_regex_misuse(r"\w+Mode").unwrap();
        assert!(hint.contains("regex escape"), "{hint}");
        assert!(hint.contains("$VAR"), "{hint}");
    }

    #[test]
    fn flags_backslash_d_escape() {
        assert!(detect_regex_misuse(r"id\d+").is_some());
    }

    #[test]
    fn does_not_flag_legitimate_backslash() {
        // \n / \t are not the regex character-class escapes we flag; agents
        // legitimately use these in string literals.
        assert!(detect_regex_misuse("\"line\\n\"").is_none());
    }

    // ----- regex misuse: character classes ---------------------------------

    #[test]
    fn flags_character_class_alpha_range() {
        let hint = detect_regex_misuse("[a-z]+Mode").unwrap();
        assert!(hint.contains("character class"), "{hint}");
    }

    #[test]
    fn flags_character_class_digit_range() {
        assert!(detect_regex_misuse("v[0-9]+").is_some());
    }

    #[test]
    fn does_not_flag_array_index() {
        // `arr[0]` is NOT a character class — `[` is followed by `0]`, not
        // by `lo-hi`.
        assert!(detect_regex_misuse("arr[0]").is_none());
        assert!(detect_regex_misuse("$A[0]").is_none());
    }

    #[test]
    fn does_not_flag_string_index() {
        assert!(detect_regex_misuse("map[\"key\"]").is_none());
    }

    // ----- regex misuse: wildcards -----------------------------------------

    #[test]
    fn flags_dot_star_in_identifier() {
        let hint = detect_regex_misuse("func.*build").unwrap();
        assert!(hint.contains("regex wildcards"), "{hint}");
        assert!(hint.contains("$$$"), "{hint}");
    }

    #[test]
    fn flags_dot_plus_in_identifier() {
        assert!(detect_regex_misuse("foo.+bar").is_some());
    }

    #[test]
    fn does_not_flag_dot_star_with_meta_var() {
        // The agent already used $ — they're in AST mode. Don't double-warn.
        assert!(detect_regex_misuse("$X.*").is_none());
    }

    #[test]
    fn does_not_flag_call_with_meta_var() {
        assert!(detect_regex_misuse("console.log($MSG)").is_none());
    }

    // ----- regex misuse: alternation (today's bug) -------------------------

    #[test]
    fn flags_text_alternation() {
        let hint = detect_regex_misuse("watch|WatchMode|--watch").unwrap();
        assert!(hint.contains("|"), "{hint}");
        assert!(hint.contains("alternation") || hint.contains("alternative"));
    }

    #[test]
    fn flags_camel_case_alternation() {
        assert!(detect_regex_misuse("noEmit|NoEmit").is_some());
    }

    /// THIS IS THE BUG THAT HIT ME ON 2026-05-06.
    /// Rust match-arm pipe alternation looks like a normal Rust pattern but
    /// ast-grep returns zero matches against source that obviously contains
    /// `LangId::C`, `LangId::Cpp`, etc. We need to flag it.
    #[test]
    fn flags_rust_match_arm_pipe_alternation() {
        let pattern = "LangId::C | LangId::Cpp | LangId::Zig | LangId::CSharp | LangId::Bash";
        let hint = detect_regex_misuse(pattern);
        assert!(
            hint.is_some(),
            "must flag Rust match-arm `|` alternation: {pattern}"
        );
        let hint = hint.unwrap();
        assert!(hint.contains("|"), "{hint}");
        assert!(
            hint.to_lowercase().contains("ast")
                || hint.to_lowercase().contains("alternation")
                || hint.to_lowercase().contains("alternative"),
            "hint should explain the AST-vs-text distinction: {hint}"
        );
    }

    #[test]
    fn flags_path_alternation() {
        assert!(detect_regex_misuse("std::fmt | std::io").is_some());
        assert!(detect_regex_misuse("foo::Bar | foo::Baz").is_some());
    }

    #[test]
    fn does_not_flag_meta_var_or_pipe() {
        // `$A | $B` — agent is in AST mode (bitwise-or pattern).
        assert!(detect_regex_misuse("$A | $B").is_none());
        assert!(detect_regex_misuse("$X.foo | $Y.bar").is_none());
    }

    #[test]
    fn does_not_flag_rust_closure() {
        // `|x| x + 1` — Rust closure starts with `|`, not alternation.
        assert!(detect_regex_misuse("|x| x + 1").is_none());
        assert!(detect_regex_misuse("|args| { $$$ }").is_none());
    }

    #[test]
    fn does_not_flag_pipe_inside_string() {
        // `console.log("foo|bar")` — `|` is inside a string literal, not a
        // pattern operator.
        assert!(detect_regex_misuse("console.log(\"foo|bar\")").is_none());
    }

    // ----- legitimate AST patterns must NOT be flagged ---------------------

    #[test]
    fn allows_function_with_body() {
        assert!(detect_regex_misuse("function $NAME($$$) { $$$ }").is_none());
    }

    #[test]
    fn allows_console_log() {
        assert!(detect_regex_misuse("console.log($$$)").is_none());
    }

    #[test]
    fn allows_python_def() {
        assert!(detect_regex_misuse("def $FUNC($$$)").is_none());
    }

    #[test]
    fn allows_match_with_meta_vars() {
        assert!(detect_regex_misuse("match $X { $$$ }").is_none());
    }

    // ----- language-specific shape mistakes --------------------------------

    #[test]
    fn flags_python_def_trailing_colon() {
        let hint =
            detect_language_specific_mistake("def $FUNC($$$):", &AstGrepLang::Python).unwrap();
        assert!(hint.contains("body") || hint.contains("colon"), "{hint}");
        assert!(hint.contains("def $FUNC($$$)"), "{hint}");
    }

    #[test]
    fn flags_python_class_trailing_colon() {
        let hint = detect_language_specific_mistake("class $C:", &AstGrepLang::Python).unwrap();
        assert!(hint.contains("class") || hint.contains("body"), "{hint}");
    }

    #[test]
    fn flags_async_def_trailing_colon() {
        assert!(
            detect_language_specific_mistake("async def $F($$$):", &AstGrepLang::Python).is_some()
        );
    }

    #[test]
    fn flags_ts_function_name_only() {
        let hint =
            detect_language_specific_mistake("function $NAME", &AstGrepLang::TypeScript).unwrap();
        assert!(hint.contains("params and body"), "{hint}");
    }

    #[test]
    fn flags_ts_export_async_function_name_only() {
        assert!(detect_language_specific_mistake(
            "export async function $NAME",
            &AstGrepLang::TypeScript
        )
        .is_some());
    }

    #[test]
    fn flags_go_func_name_only() {
        let hint = detect_language_specific_mistake("func $NAME", &AstGrepLang::Go).unwrap();
        assert!(hint.contains("Go"), "{hint}");
        assert!(hint.contains("func $NAME($$$) { $$$ }"), "{hint}");
    }

    #[test]
    fn flags_rust_fn_name_only() {
        let hint = detect_language_specific_mistake("fn $NAME", &AstGrepLang::Rust).unwrap();
        assert!(hint.contains("Rust"), "{hint}");
        assert!(hint.contains("fn $NAME($$$) { $$$ }"), "{hint}");
    }

    #[test]
    fn flags_rust_pub_fn_name_only() {
        assert!(detect_language_specific_mistake("pub fn $NAME", &AstGrepLang::Rust).is_some());
        assert!(
            detect_language_specific_mistake("pub async fn $NAME", &AstGrepLang::Rust).is_some()
        );
    }

    #[test]
    fn allows_complete_function_patterns() {
        assert!(detect_language_specific_mistake(
            "function $NAME($$$) { $$$ }",
            &AstGrepLang::TypeScript
        )
        .is_none());
        assert!(
            detect_language_specific_mistake("fn $NAME($$$) { $$$ }", &AstGrepLang::Rust).is_none()
        );
        assert!(
            detect_language_specific_mistake("def $FUNC($$$):\n    $$$", &AstGrepLang::Python)
                .is_none()
        );
    }

    // ----- combined detect_pattern_hint ------------------------------------

    #[test]
    fn regex_misuse_takes_precedence_over_lang_specific() {
        // `foo|bar` is alternation AND looks vaguely like a TS pattern. The
        // regex-misuse detector wins — it's the more useful diagnosis.
        let hint = detect_pattern_hint("foo|bar", &AstGrepLang::TypeScript).unwrap();
        assert!(hint.to_lowercase().contains("|") || hint.contains("alternation"));
    }

    #[test]
    fn pattern_with_no_issue_returns_none() {
        assert!(
            detect_pattern_hint("function $NAME($$$) { $$$ }", &AstGrepLang::TypeScript).is_none()
        );
        assert!(detect_pattern_hint("LangId::Bash", &AstGrepLang::Rust).is_none());
    }

    // ----- helper coverage -------------------------------------------------

    #[test]
    fn strips_string_literal_with_pipes() {
        let stripped = strip_string_literals("a + \"foo|bar\" + b");
        assert!(!stripped.contains("foo"));
        assert!(!stripped.contains('|'));
    }

    #[test]
    fn strips_handles_escaped_quote() {
        let stripped = strip_string_literals(r#"x = "\"|" + y"#);
        // The `|` was inside the string; should not survive.
        assert!(!stripped.contains('|'));
    }

    #[test]
    fn name_only_meta_var_recognizes_meta_vars() {
        assert!(name_only_meta_var("$NAME"));
        assert!(name_only_meta_var("$NAME "));
        assert!(name_only_meta_var("$N1"));
    }

    #[test]
    fn name_only_meta_var_rejects_call_or_block() {
        assert!(!name_only_meta_var("$NAME($$$)"));
        assert!(!name_only_meta_var("$NAME { $$$ }"));
        assert!(!name_only_meta_var("$NAME<T>"));
    }
}
