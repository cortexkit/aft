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
        AstGrepLang::Nix => {
            // An attrset body (`a = 1; b = 2;` / `a = 1; inherit b;`) is several
            // sibling expressions with no single AST node to bind, so the
            // contextual fallback refuses it. Scope the pattern to one member.
            if nix_pattern_has_multiple_bindings(src) {
                return Some(
                    "Hint: A nix pattern must contain one binding or one inherit clause \
                     (e.g. \"$NAME = $VALUE;\" or \"inherit $NAME;\"). An attrset body is \
                     several expressions with no single AST node to match, so it will not \
                     parse — search for one binding at a time, or match the enclosing \
                     attrset expression."
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

/// Advance past a Nix `/* … */` block comment, called with `bytes[*i] == b'/'`
/// and `bytes[*i + 1] == b'*'`. Comments do not nest; an unterminated comment
/// runs to the end. Shared by the hint scanner and the pattern comment strip
/// so the two terminators cannot drift.
pub(crate) fn nix_skip_block_comment(bytes: &[u8], i: &mut usize) {
    *i += 2;
    while *i + 1 < bytes.len() && !(bytes[*i] == b'*' && bytes.get(*i + 1) == Some(&b'/')) {
        *i += 1;
    }
    *i = if *i + 1 < bytes.len() {
        *i + 2
    } else {
        bytes.len()
    };
}

/// True when an apostrophe pair at `i` continues a Nix identifier token.
///
/// The vendored grammar lexes identifiers as `/[a-zA-Z_][a-zA-Z0-9_'\-]*/`,
/// so a quote pair is identifier content whenever an identifier token is
/// live at `i`: started before (`foo''`, `foo-''`), or restarted by the
/// lexer at a numeric boundary or after an operator/select (`9x''`,
/// `-b''`, `a.foo''`, `a+foo''`, `1.5e2-b''`). The pair opens a string
/// instead when the token behind it is not an identifier: a path (only a
/// real `/` proves one — scanner.c needs a separator, path chars exclude
/// `'`), or a float token (`1.5e2''`, `.5e2''`: float_expression
/// grammar.js:55-56 swallows its own exponent and admits neither letter
/// nor `'`; its select lookalike `0.e2''` is NOT a float — branch 1 needs
/// a leading 1-9, branch 2 needs a digit after the dot — so there the
/// identifier restarts). A `*/` flush behind the run counts as a
/// comment terminator (extras boundary, fresh token) only when the lex-state
/// table shows it really closes a block comment (`nix_lex_code_states`):
/// a bare `X*/Y` is times-then-path, which opens.
/// Residual: `uri_expression` tails (grammar.js:83) admit `'`, so a
/// spaceless `http://x''` opens a string here while the grammar continues
/// the URI; spaceless URI juxtaposition is degenerate in patterns.
/// Positions inside `${ … }` bodies classify as code (bodies are full
/// expressions, grammar.js:386), except bytes covered by body comments
/// or nested strings.
///
/// Linearity: a lex pass consults this guard at every quote pair, and the
/// facts it needs all live behind the pair. `NixApostropheGuard` therefore
/// precomputes the identifier-run boundary and a run "has a start char" bit
/// in one forward pass, and memoizes the first run verdict that a scan
/// resolves at a terminal byte (rather than at the pair itself). Quote pairs
/// inside one long identifier then cost O(1) each — the rescans this replaces
/// made an apostrophe-heavy identifier quadratic.
pub(crate) struct NixApostropheGuard {
    /// `run_start[i]`: start of the maximal identifier-content run ending at
    /// `i - 1`; `i` itself when the byte at `i - 1` cannot continue one.
    run_start: Vec<u32>,
    /// Whether `bytes[run_start[i]..i]` holds a letter or underscore.
    has_start: Vec<bool>,
    /// Memo: verdict for the run that starts at `memo_j`, valid once
    /// `memo_ok`. A scan cut short by reaching the pair position is not
    /// cached; a later pair in the same run repeats it, so each run pays for
    /// at most one unresolved scan.
    memo_j: usize,
    memo_verdict: bool,
    memo_ok: bool,
    /// Memo key check: a path decision consults `states`, so a cached verdict
    /// is only reusable while the table behind the run has not been (re)built
    /// — callers that construct a fresh table overwrite any earlier one.
    memo_states: *const bool,
}

fn nix_is_ident_cont(c: u8) -> bool {
    matches!(c, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'\'' | b'-')
}

fn nix_is_ident_start(c: u8) -> bool {
    matches!(c, b'a'..=b'z' | b'A'..=b'Z' | b'_')
}

/// Identifier restart after an operator inside a run: digits are then a
/// number token, an identifier start continues, a second dash restarts
/// again; anything else opens (a `+` here would be unary plus, which
/// the grammar does not admit, so no valid pattern reaches it). The second
/// element says whether the scan resolved at a terminal byte (`true`) or
/// ran out at the pair position (`false`, verdict may differ at a later pair).
fn nix_restarts_after_dash(bytes: &[u8], from: usize, i: usize) -> (bool, bool) {
    let mut p = from;
    loop {
        while p < i && bytes[p].is_ascii_digit() {
            p += 1;
        }
        if p >= i {
            return (false, false);
        }
        if nix_is_ident_start(bytes[p]) {
            return (true, true);
        }
        if bytes[p] != b'-' {
            return (false, true);
        }
        p += 1;
    }
}

impl NixApostropheGuard {
    pub(crate) fn new(bytes: &[u8]) -> Self {
        let n = bytes.len();
        let mut run_start = vec![0u32; n];
        let mut has_start = vec![false; n];
        // Identifier tokens are ASCII-only, so runs over raw bytes match the
        // byte classification the scanner's identifier regex admits.
        for i in 1..n {
            if nix_is_ident_cont(bytes[i - 1]) {
                if i >= 2 && nix_is_ident_cont(bytes[i - 2]) {
                    run_start[i] = run_start[i - 1];
                    has_start[i] = has_start[i - 1] || nix_is_ident_start(bytes[i - 1]);
                } else {
                    run_start[i] = (i - 1) as u32;
                    has_start[i] = nix_is_ident_start(bytes[i - 1]);
                }
            } else {
                run_start[i] = i as u32;
            }
        }
        Self {
            run_start,
            has_start,
            memo_j: usize::MAX,
            memo_verdict: false,
            memo_ok: false,
            memo_states: std::ptr::null(),
        }
    }

    /// Same contract as the byte scan it replaces: `states` is the pass's
    /// lex-state table (positions behind `i` are already final, so a cached
    /// run verdict cannot go stale against a table that keeps being written
    /// ahead of the pair).
    pub(crate) fn continues(&mut self, bytes: &[u8], i: usize, states: &[bool]) -> bool {
        let j = self.run_start[i] as usize;
        if j == i {
            return false; // previous byte cannot be part of an identifier
        }
        let key = states.as_ptr();
        if self.memo_j != j || !self.memo_ok || self.memo_states != key {
            let (verdict, resolved) = self.scan_run(bytes, i, j, self.has_start[i], states);
            self.memo_j = j;
            self.memo_verdict = verdict;
            self.memo_ok = resolved;
            self.memo_states = key;
            return verdict;
        }
        self.memo_verdict
    }

    /// The original backward decision, with `j` (run start) and `has` (run
    /// holds an identifier start) carried in instead of rescanned. Second
    /// return element: the scan settled on a terminal byte, so every later
    /// pair in the same run shares `verdict`.
    fn scan_run(
        &self,
        bytes: &[u8],
        i: usize,
        j: usize,
        has: bool,
        states: &[bool],
    ) -> (bool, bool) {
        // Float mantissa immediately behind the run (grammar.js:56):
        // branch 1 `[1-9][0-9]*.` (digit after dot optional) or
        // branch 2 `0?\.[0-9]+` (digit after dot required).
        if j > 0 && bytes[j - 1] == b'.' {
            let mut d0 = j - 1;
            while d0 > 0 && bytes[d0 - 1].is_ascii_digit() {
                d0 -= 1;
            }
            let digit_before = d0 < j - 1;
            let digit_after = bytes[j].is_ascii_digit();
            let float = if digit_before {
                bytes[d0] != b'0' || digit_after
            } else {
                digit_after
            };
            if float {
                // A complete float tail (`digits`, optional `e[+-]?digits`,
                // possibly followed by `-` and a fresh token) is a number;
                // its letters belong to the number unless an identifier
                // restarts after a dash.
                let mut t = j;
                while t < i && bytes[t].is_ascii_digit() {
                    t += 1;
                }
                if t == i {
                    return (false, false);
                }
                if matches!(bytes[t], b'e' | b'E') {
                    let mut u = t + 1;
                    if u < i && matches!(bytes[u], b'+' | b'-') {
                        u += 1;
                    }
                    let exponent_digits = u;
                    while u < i && bytes[u].is_ascii_digit() {
                        u += 1;
                    }
                    if exponent_digits < u {
                        if u == i {
                            return (false, false);
                        }
                        if bytes[u] == b'-' {
                            return nix_restarts_after_dash(bytes, u + 1, i);
                        }
                    }
                }
            }
            // Not a float (or the tail stayed open): fall through, the dot
            // is a select boundary.
        }
        // Extend back over path/operator chars. `/` proves a path token
        // unless it truly terminates a block comment (extras boundary: the
        // identifier behind it is a fresh token, stop scanning there).
        let mut k = j;
        while k > 0 {
            let c = bytes[k - 1];
            if c == b'/' {
                // Lex-state lookup: a code-level slash is a path fragment or a
                // division operand and a path cannot contain `'`, so the pair
                // opens. Anything else at that byte (comment text — including
                // a `*/` terminator — or string material) hides the slash from
                // the lexer; the run is judged on its own from here.
                if states.get(k - 1) == Some(&true) {
                    return (false, true);
                }
                break;
            }
            if matches!(c, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'+' | b'.') {
                k -= 1;
                continue;
            }
            break;
        }
        // An identifier may restart inside the run (after a `-` or digits);
        // without any start char (letter/underscore) there is nothing to
        // continue and the pair opens a string.
        (has, true)
    }
}

/// Per-byte lex states for a Nix pattern: `true` marks a byte the lexer
/// reads as code (ordinary source, including the full expressions inside
/// `${ … }` interpolation bodies), `false` marks comment text, string
/// content, and escape spans. One forward pass with the shared frame model:
/// comments are lexer extras (grammar.js:25) and strings are regions that
/// can hide anything, so the apostrophe guard asks this table instead of
/// rescanning — a table lookup cannot recurse, which is what keeps guard
/// checks linear on patterns with many `*/x''` elements.
pub(crate) fn nix_lex_code_states(bytes: &[u8]) -> Vec<bool> {
    let mut st = vec![true; bytes.len()];
    let mut guard = NixApostropheGuard::new(bytes);
    let mut stack: Vec<NixStrFrame> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if stack.is_empty() {
            if b == b'#' {
                st[i] = false;
                i += 1;
                while i < bytes.len() && bytes[i] != b'\n' {
                    st[i] = false;
                    i += 1;
                }
                continue;
            }
            if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    st[i] = false;
                    i += 1;
                }
                for pad in [i, i + 1] {
                    if pad < bytes.len() {
                        st[pad] = false;
                    }
                }
                i += 2;
                continue;
            }
            if b == b'"' {
                st[i] = false;
                stack.push(NixStrFrame {
                    indented: false,
                    braces: 0,
                });
                i += 1;
                continue;
            }
            if b == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                if guard.continues(bytes, i, &st) {
                    // Identifier material (`9x''z`, `foo''`): not an opener.
                    i += 1;
                    continue;
                }
                st[i] = false;
                st[i + 1] = false;
                stack.push(NixStrFrame {
                    indented: true,
                    braces: 0,
                });
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        let top = *stack.last().expect("checked non-empty");
        if top.braces > 0 {
            // Interpolation body: a full expression (grammar.js:386), so
            // these bytes are code; comments and nested strings inside the
            // body still hide their own material.
            if b == b'#' {
                st[i] = false;
                i += 1;
                while i < bytes.len() && bytes[i] != b'\n' {
                    st[i] = false;
                    i += 1;
                }
                continue;
            }
            if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    st[i] = false;
                    i += 1;
                }
                for pad in [i, i + 1] {
                    if pad < bytes.len() {
                        st[pad] = false;
                    }
                }
                i += 2;
                continue;
            }
            if b == b'$' && bytes.get(i + 1) == Some(&b'{') {
                stack.last_mut().unwrap().braces += 1;
                i += 2;
                continue;
            }
            if b == b'{' {
                stack.last_mut().unwrap().braces += 1;
                i += 1;
                continue;
            }
            if b == b'}' {
                stack.last_mut().unwrap().braces = (stack.last_mut().unwrap().braces - 1).max(0);
                i += 1;
                continue;
            }
            if b == b'"' {
                st[i] = false;
                stack.push(NixStrFrame {
                    indented: false,
                    braces: 0,
                });
                i += 1;
                continue;
            }
            if b == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                if guard.continues(bytes, i, &st) {
                    i += 1; // identifier content inside the body
                    continue;
                }
                st[i] = false;
                st[i + 1] = false;
                stack.push(NixStrFrame {
                    indented: true,
                    braces: 0,
                });
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        // String content: not code until the frame closes or an interpolation
        // body opens.
        st[i] = false;
        if top.indented {
            if b == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                match bytes.get(i + 2) {
                    // scanner.c escape set, aligned with nix_string_step:
                    // `''\x` (incl. `''\$`) is a four-byte escape.
                    Some(b'\\') => {
                        let span = if i + 3 < bytes.len() { 4 } else { 3 };
                        for pad in i..(i + span).min(bytes.len()) {
                            st[pad] = false;
                        }
                        i += span;
                    }
                    Some(b'\'') | Some(b'$') => {
                        for pad in i..(i + 3).min(bytes.len()) {
                            st[pad] = false;
                        }
                        i += 3;
                    }
                    _ => {
                        stack.pop();
                        st[i + 1] = false; // second byte of the closing ''
                        i += 2;
                    }
                }
                continue;
            }
            if b == b'$' {
                // Mirror nix_string_step / scanner.c: `$` swallows the next
                // char unless it stops there, so only an ODD dollar-run
                // before `{` interpolates (`''$${x}''` is literal).
                match bytes.get(i + 1) {
                    Some(b'{') => {
                        stack.last_mut().unwrap().braces += 1;
                        st[i + 1] = false; // the opener brace is string syntax
                        i += 2;
                    }
                    Some(b'\'') => i += 1, // a quote pair must stop the fragment
                    Some(_) => {
                        st[i + 1] = false;
                        i += 2;
                    }
                    None => i += 1,
                }
                continue;
            }
            i += 1;
            continue;
        }
        if b == b'\\' {
            if i + 1 < bytes.len() {
                st[i + 1] = false;
            }
            i += 2;
            continue;
        }
        if b == b'$' {
            // scanner.c: `$` stops before `{` (interpolation), `"` and `\`
            // (delimiter/escape), and otherwise swallows one more char, so
            // `$${` does not interpolate while `$$${` does.
            match bytes.get(i + 1) {
                Some(b'{') => {
                    stack.last_mut().unwrap().braces += 1;
                    st[i + 1] = false; // the opener brace is string syntax
                    i += 2;
                }
                Some(b'"') | Some(b'\\') => i += 1,
                Some(_) => {
                    st[i + 1] = false;
                    i += 2;
                }
                None => i += 1,
            }
            continue;
        }
        if b == b'"' {
            stack.pop();
            i += 1;
            continue;
        }
        i += 1;
    }
    st
}

/// One open Nix string: whether it is an indented `'' … ''` and the current
/// interpolation/brace depth inside it. An interpolation body is a full
/// expression (`interpolation: seq("${", _expression, "}")` in the vendored
/// grammar), so strings legitimately nest inside `${ … }`.
#[derive(Clone, Copy)]
pub(crate) struct NixStrFrame {
    pub indented: bool,
    pub braces: i32,
}

/// Advance one atomic span starting at `bytes[i]` inside a (possibly nested)
/// Nix string; the caller advances by the returned length and repeats until
/// its stack is empty. Pushes a frame when a nested string opens, pops when
/// the current string closes. Precondition: `stack` is non-empty and
/// `bytes[i]` lies inside the top string. `states` is the pattern's lex-state
/// table (`nix_lex_code_states`), consulted for apostrophe pairs in
/// interpolation bodies. A span may end mid-glyph (escapes), so callers that
/// slice must copy per REGION, not per span.
pub(crate) fn nix_string_step(
    bytes: &[u8],
    i: usize,
    stack: &mut Vec<NixStrFrame>,
    states: &[bool],
    guard: &mut NixApostropheGuard,
) -> usize {
    let Some(top) = stack.last_mut() else {
        return 1;
    };
    let b = bytes[i];
    if top.braces > 0 {
        // Interpolation body: a full expression, so comments are legal
        // anywhere. Skip them before reading quotes or braces so comment
        // text cannot forge a string-open or brace-close event.
        if b == b'#' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            return j - i;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut j = i;
            nix_skip_block_comment(bytes, &mut j);
            return j - i;
        }
        match b {
            b'"' => {
                stack.push(NixStrFrame {
                    indented: false,
                    braces: 0,
                });
                return 1;
            }
            b'\'' if bytes.get(i + 1) == Some(&b'\'') => {
                if guard.continues(bytes, i, states) {
                    return 1; // identifier content (`x''y`), not an open
                }
                stack.push(NixStrFrame {
                    indented: true,
                    braces: 0,
                });
                return 2;
            }
            b'{' => top.braces += 1,
            b'}' => top.braces = (top.braces - 1).max(0),
            _ => {}
        }
        return 1;
    }
    if top.indented {
        if b == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                // Two quotes always stop indented content: either the close
                // delimiter or one of the `'''` / `''$` / `''\x` escapes
                // (scanner.c scan_indented_string_fragment).
                return match bytes.get(i + 2) {
                    Some(&b'\'') => 3,                        // `'''` literal apostrophe
                    Some(&b'$') => 3, // `''$` literal dollar, so `''${` is literal `${`
                    Some(&b'\\') if i + 3 < bytes.len() => 4, // `''\$` and `''\x`
                    Some(&b'\\') => 3, // `''\` at end of input
                    _ => {
                        stack.pop();
                        2 // closing `''`
                    }
                };
            }
            return 1; // lone `'` inside indented content
        }
        if b == b'$' {
            // Bare `${` interpolates inside an indented string; `$` plus
            // any other char (except a following quote pair, which stops
            // as a delimiter/escape) is content.
            return match bytes.get(i + 1) {
                Some(&b'{') => {
                    top.braces += 1;
                    2
                }
                Some(&b'\'') => 1,
                Some(_) => 2,
                None => 1,
            };
        }
        return 1;
    }
    if b == b'\\' {
        return if i + 1 < bytes.len() { 2 } else { 1 };
    }
    if b == b'$' {
        // scanner.c: `$` stops before `{` (interpolation), `"` and `\`
        // (delimiter/escape), and otherwise swallows one more char, so
        // `$${` does not interpolate while `$$${` does.
        return match bytes.get(i + 1) {
            Some(&b'{') => {
                top.braces += 1;
                2
            }
            Some(&b'"') | Some(&b'\\') => 1,
            Some(_) => 2,
            None => 1,
        };
    }
    if b == b'"' {
        stack.pop();
    }
    1
}

/// A Nix attrset body carries several sibling members (`a = 1; b = 2;` or
/// `a = 1; inherit b;`), so the contextual fallback has no single node to bind
/// and refuses the pattern. The shape reads as a top-level `;` with a further
/// binding-shaped member after it. String contents (including `${ … }`
/// interpolation bodies, where strings nest) and comments are skipped so a `;`
/// inside `"x;y"`, `"${ f "x;y" }"`, `# …; …`, or `/* …; … */` is not mistaken
/// for a binding terminator.
fn nix_pattern_has_multiple_bindings(src: &str) -> bool {
    let bytes = src.as_bytes();
    let states = nix_lex_code_states(bytes);
    let mut guard = NixApostropheGuard::new(bytes);
    let mut depth: i32 = 0;
    let mut strings: Vec<NixStrFrame> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !strings.is_empty() {
            i += nix_string_step(bytes, i, &mut strings, &states, &mut guard);
            continue;
        }
        // Top level: comments hide their contents; a `;` inside one must not
        // look like a member terminator.
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            nix_skip_block_comment(bytes, &mut i);
            continue;
        }
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        match b {
            // Consume both opening quotes; escape and interpolation handling
            // then lives in the shared frame stepper. An apostrophe pair
            // right after an identifier char continues the identifier.
            b'\'' if bytes.get(i + 1) == Some(&b'\'') => {
                if guard.continues(bytes, i, &states) {
                    i += 1;
                    continue;
                }
                strings.push(NixStrFrame {
                    indented: true,
                    braces: 0,
                });
                i += 2;
                continue;
            }
            b'"' => strings.push(NixStrFrame {
                indented: false,
                braces: 0,
            }),
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => depth = (depth - 1).max(0),
            b';' if depth == 0 => {
                // The next member can hide behind whitespace, `#` line
                // comments, `/* … */` block comments, or open as a quoted
                // attribute name (`"a" = 1;`).
                let mut j = i + 1;
                while j < bytes.len() {
                    let c = bytes[j];
                    if c.is_ascii_whitespace() {
                        j += 1;
                    } else if c == b'#' {
                        while j < bytes.len() && bytes[j] != b'\n' {
                            j += 1;
                        }
                    } else if c == b'/' && bytes.get(j + 1) == Some(&b'*') {
                        nix_skip_block_comment(bytes, &mut j);
                    } else {
                        break;
                    }
                }
                if let Some(&c) = bytes.get(j) {
                    // A top-level `let …; in …` expression is one member: the
                    // `in` that terminates the let is not a sibling binding.
                    if c == b'i'
                        && bytes.get(j + 1) == Some(&b'n')
                        && !matches!(bytes.get(j + 2),
                            Some(w) if w.is_ascii_alphanumeric() || *w == b'_')
                    {
                        i = j + 2;
                        continue;
                    }
                    if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c == b'"' || c == b'\''
                    {
                        return true;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Recognize compact regex ranges, not subtraction inside an AST subscript.
/// A receiver followed by a terminal index is valid code even when the index
/// resembles a range; only unmistakable character-class shapes warrant advice.
fn has_char_class_range(src: &str) -> bool {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        let Some(close) = bytes[i + 1..]
            .iter()
            .position(|byte| *byte == b']')
            .map(|pos| i + 1 + pos)
        else {
            break;
        };
        let prefix = src[..i].trim_end();
        let suffix = src[close + 1..].trim_start();
        let receiver = prefix
            .as_bytes()
            .last()
            .is_some_and(|byte| is_ascii_word(*byte) || matches!(byte, b')' | b']'));
        let subscript_tail = suffix.is_empty()
            || suffix.as_bytes().first().is_some_and(|byte| {
                matches!(byte, b'.' | b'[' | b')' | b']' | b',' | b';' | b'=' | b'}')
            });
        if receiver && subscript_tail {
            i = close + 1;
            continue;
        }
        let mut j = i + 1;
        if bytes.get(j) == Some(&b'^') {
            j += 1;
        }
        let mut range = false;
        let mut valid = true;
        while j < close {
            if j + 2 < close && bytes[j + 1] == b'-' {
                let a = bytes[j];
                let b = bytes[j + 2];
                let same_family = (a.is_ascii_lowercase() && b.is_ascii_lowercase())
                    || (a.is_ascii_uppercase() && b.is_ascii_uppercase())
                    || (a.is_ascii_digit() && b.is_ascii_digit());
                if !same_family || a > b {
                    valid = false;
                    break;
                }
                range = true;
                j += 3;
            } else if is_ascii_alnum(bytes[j]) || bytes[j] == b'_' {
                j += 1;
            } else {
                valid = false;
                break;
            }
        }
        if valid && range {
            return true;
        }
        i = close + 1;
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

#[cfg(test)]
mod nix_guard_linear_time {
    use super::*;
    use std::time::{Duration, Instant};

    /// One identifier of 30 000 apostrophe pairs: `a''''''…`. Every pair is a
    /// live quote pair at lex time, so a guard that rescans the identifier
    /// behind each pair costs quadratic time on exactly this input.
    fn apostrophe_heavy_identifier(pairs: usize) -> String {
        let mut s = String::from("a");
        for _ in 0..pairs {
            s.push_str("''");
        }
        s
    }

    #[test]
    fn apostrophe_heavy_identifier_lexes_without_quadratic_rescan() {
        let pattern = apostrophe_heavy_identifier(30_000);
        // Warm run: the identifier is all identifier material — no byte is
        // ever comment text or string content.
        let states = nix_lex_code_states(pattern.as_bytes());
        assert_eq!(states.len(), pattern.len());
        assert!(states.iter().all(|&code| code));
        let start = Instant::now();
        let states = nix_lex_code_states(pattern.as_bytes());
        let elapsed = start.elapsed();
        let width = pattern.len();
        assert!(states.iter().all(|&code| code));
        assert!(
            elapsed < Duration::from_millis(500),
            "apostrophe guard rescanned the identifier: {width} bytes took {elapsed:?} — lexing a pattern must stay linear"
        );
    }
}

#[cfg(test)]
mod approved_audit11 {
    use super::*;

    #[test]
    fn subtraction_subscripts_do_not_get_character_class_hints() {
        for pattern in [
            "arr[i-1]",
            "$A[$I-1]",
            "arr[x-y]",
            "arr[index-1]",
            "arr[i - 1]",
            "arr[0-9]",
            "[i-1]",
        ] {
            assert!(detect_regex_misuse(pattern).is_none(), "{pattern}");
        }
        for pattern in ["[a-z]", "[0-9]", "[A-Za-z0-9]", "[a-z]+", "foo[a-z]bar"] {
            assert!(detect_regex_misuse(pattern).is_some(), "{pattern}");
        }
    }
}
