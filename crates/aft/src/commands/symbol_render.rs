use std::path::Path;

use crate::commands::outline::{
    build_outline_tree, format_entry_with_sig, symbol_to_entry, OutlineEntry,
};
use crate::context::AppContext;
use crate::parser::LangId;
use crate::symbols::{Range, Symbol, SymbolKind};

pub const LARGE_CONTAINER_MENU_LINE_THRESHOLD: usize = 150;

pub struct ContainerOutline {
    entry: OutlineEntry,
    symbols: Vec<Symbol>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetedSymbolRenderStatus {
    Complete,
    Truncated,
    Menu,
}

pub struct BudgetedSymbolRender {
    pub content: String,
    pub status: BudgetedSymbolRenderStatus,
}

pub fn symbol_kind_string(kind: &SymbolKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(String::from))
        .unwrap_or_else(|| format!("{kind:?}").to_lowercase())
}

pub fn qualified_symbol_name(symbol: &Symbol) -> String {
    let mut parts = symbol
        .scope_chain
        .iter()
        .filter(|part| !part.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    parts.push(symbol.name.clone());
    parts.join(".")
}

pub fn might_have_container_members(symbol: &Symbol) -> bool {
    matches!(
        &symbol.kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::Variable
            | SymbolKind::TypeAlias
    )
}

fn is_container_kind(kind: &SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Interface | SymbolKind::Enum
    )
}

pub fn build_container_outline(
    ctx: &AppContext,
    resolved_file_path: &Path,
    target: &Symbol,
) -> Result<ContainerOutline, crate::error::AftError> {
    let symbols = ctx.provider().list_symbols(resolved_file_path)?;
    let entries = build_outline_tree(&symbols);
    let entry = find_outline_entry(&entries, target)
        .cloned()
        .unwrap_or_else(|| symbol_to_entry(target));
    Ok(ContainerOutline { entry, symbols })
}

fn find_outline_entry<'a>(
    entries: &'a [OutlineEntry],
    target: &Symbol,
) -> Option<&'a OutlineEntry> {
    for entry in entries {
        if entry.name == target.name && entry.range == target.range {
            return Some(entry);
        }
        if let Some(found) = find_outline_entry(&entry.members, target) {
            return Some(found);
        }
    }
    None
}

pub fn should_return_member_menu(
    target: &Symbol,
    lang: Option<LangId>,
    outline: Option<&ContainerOutline>,
) -> bool {
    let Some(outline) = outline else {
        return false;
    };
    let is_container = is_container_kind(&target.kind) || !outline.entry.members.is_empty();
    if !is_container {
        return false;
    }

    container_rendered_line_count(target, lang, &outline.entry)
        > LARGE_CONTAINER_MENU_LINE_THRESHOLD
}

fn range_line_count(range: &Range) -> usize {
    range
        .end_line
        .saturating_sub(range.start_line)
        .saturating_add(1) as usize
}

fn range_contains(outer: &Range, inner: &Range) -> bool {
    (outer.start_line, outer.start_col) <= (inner.start_line, inner.start_col)
        && (outer.end_line, outer.end_col) >= (inner.end_line, inner.end_col)
}

fn container_rendered_line_count(
    target: &Symbol,
    lang: Option<LangId>,
    entry: &OutlineEntry,
) -> usize {
    let mut line_count = range_line_count(&target.range);

    // Rust impl blocks are associated with the type in the outline symbol model,
    // but their method ranges sit outside the struct/enum/trait declaration.
    // Count those associated method spans so behavior-heavy Rust types get a
    // drill-down menu without introducing a separate `impl` zoom target.
    if lang == Some(LangId::Rust) {
        for member in &entry.members {
            if !range_contains(&target.range, &member.range) {
                line_count = line_count.saturating_add(range_line_count(&member.range));
            }
        }
    }

    line_count
}

pub fn render_container_member_menu(
    target: &Symbol,
    outline: &ContainerOutline,
    zoom_enabled: bool,
) -> String {
    let kind = symbol_kind_string(&target.kind);
    let qualified_name = qualified_symbol_name(target);
    let member_count = outline.entry.members.len();
    let action = if zoom_enabled { "zoom" } else { "read" };
    let mut lines = vec![format!(
        "{kind} {qualified_name} ({member_count} members) — member-signature menu; {action} a member for its body"
    )];

    lines.push(format_qualified_entry(&outline.entry, Some(target)));
    if outline.entry.members.is_empty() {
        lines.push("  (no direct members found)".to_string());
    } else {
        for member in &outline.entry.members {
            let symbol = find_symbol_for_entry(&outline.symbols, member);
            lines.push(format!("  .{}", format_qualified_entry(member, symbol)));
        }
    }

    lines.join("\n")
}

fn find_symbol_for_entry<'a>(symbols: &'a [Symbol], entry: &OutlineEntry) -> Option<&'a Symbol> {
    symbols
        .iter()
        .find(|symbol| symbol.name == entry.name && symbol.range == entry.range)
}

pub fn format_qualified_entry(entry: &OutlineEntry, symbol: Option<&Symbol>) -> String {
    let Some(symbol) = symbol else {
        return format_entry_with_sig(entry);
    };
    let qualified_name = qualified_symbol_name(symbol);
    if qualified_name == symbol.name {
        return format_entry_with_sig(entry);
    }

    let mut display = entry.clone();
    display.name = qualified_name.clone();
    let signature = entry.signature.as_deref().unwrap_or(entry.name.as_str());
    display.signature = Some(qualified_signature(
        &symbol.name,
        &qualified_name,
        signature,
    ));
    format_entry_with_sig(&display)
}

fn qualified_signature(name: &str, qualified_name: &str, signature: &str) -> String {
    if signature == name {
        return qualified_name.to_string();
    }

    if let Some(rest) = signature.strip_prefix(name) {
        return format!("{qualified_name}{rest}");
    }

    format!("{qualified_name} — {signature}")
}

pub fn render_symbol_within_budget(
    target: &Symbol,
    lines: &[String],
    lang: Option<LangId>,
    outline: Option<&ContainerOutline>,
    max_lines: usize,
    zoom_enabled: bool,
) -> BudgetedSymbolRender {
    if should_return_member_menu(target, lang, outline) {
        let outline = outline.expect("member menu requires an outline");
        return BudgetedSymbolRender {
            content: render_container_member_menu(target, outline, zoom_enabled),
            status: BudgetedSymbolRenderStatus::Menu,
        };
    }

    let start = (target.range.start_line as usize).min(lines.len());
    let end = ((target.range.end_line as usize) + 1).min(lines.len());
    if start >= end {
        return BudgetedSymbolRender {
            content: String::new(),
            status: BudgetedSymbolRenderStatus::Complete,
        };
    }

    let render_start = doc_comment_start(lines, start).min(end);
    let full_len = end.saturating_sub(render_start);
    if full_len <= max_lines {
        return BudgetedSymbolRender {
            content: lines[render_start..end].join("\n"),
            status: BudgetedSymbolRenderStatus::Complete,
        };
    }

    let shown = max_lines.min(full_len);
    let remaining = full_len - shown;
    let mut content = if shown == 0 {
        String::new()
    } else {
        lines[render_start..render_start + shown].join("\n")
    };
    if !content.is_empty() {
        content.push('\n');
    }
    content.push_str(&format!(
        "… +{remaining} more lines — {} {} for the full body",
        if zoom_enabled { "zoom" } else { "read" },
        target.name
    ));

    BudgetedSymbolRender {
        content,
        status: BudgetedSymbolRenderStatus::Truncated,
    }
}

/// Walk `start` (0-based index of the symbol's first body line) backwards over a
/// contiguous block of leading doc-comment / attribute / decorator lines, so the
/// rank-0 preview includes the symbol's doc the way aft_zoom does. Stops at the
/// first blank line or non-comment/non-decorator line — i.e. the previous
/// symbol's code — so it never bleeds a neighbor into the preview. Heuristic by
/// line prefix to stay language-agnostic: `//` `///` `//!` (Rust/TS/JS/Go/…),
/// `/*` `*` `*/` (block / JSDoc), Rust `#[attr]`/`#![...]`, `# ` comments
/// (Python/Ruby/Bash), `--` (Lua/SQL), and `@` (TS/Java/Python decorators).
pub fn doc_comment_start(lines: &[String], start: usize) -> usize {
    let mut s = start;
    while s > 0 {
        let prev = lines[s - 1].trim_start();
        let is_doc_or_attr = prev.starts_with("//")
            || prev.starts_with("/*")
            || prev.starts_with('*')
            || is_hash_doc_or_attr(prev)
            || prev.starts_with("--")
            || prev.starts_with('@');
        if !is_doc_or_attr {
            break;
        }
        s -= 1;
    }
    s
}

fn is_hash_doc_or_attr(line: &str) -> bool {
    if line.starts_with("#[") || line.starts_with("#![") {
        return true;
    }

    let Some(rest) = line.strip_prefix('#') else {
        return false;
    };
    let Some(first) = rest.chars().next() else {
        return true;
    };
    first.is_whitespace() && !starts_with_c_preprocessor_directive(rest.trim_start())
}

fn starts_with_c_preprocessor_directive(rest: &str) -> bool {
    let directive = rest
        .split(|ch: char| !ch.is_ascii_alphabetic())
        .next()
        .unwrap_or_default();
    matches!(
        directive,
        "define"
            | "elif"
            | "else"
            | "endif"
            | "error"
            | "if"
            | "ifdef"
            | "ifndef"
            | "include"
            | "line"
            | "pragma"
            | "region"
            | "undef"
            | "using"
            | "warning"
    )
}
