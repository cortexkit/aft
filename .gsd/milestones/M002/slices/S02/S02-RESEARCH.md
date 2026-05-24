# S02: Scope-aware Insertion & Compound Operations — Research

**Date:** 2026-03-14

## Summary

S02 delivers two requirements: R014 (scope-aware member insertion via `add_member`) and R015 (language-specific compound operations via `add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags`). The codebase is ready — S01 established the command handler pattern (param extraction → validate → backup → mutate → write → validate syntax → respond) and the tree-sitter AST infrastructure covers all needed node types.

The core technical challenge is **indentation detection** (D042). Python uses indentation as scope, so inserting a method requires matching the existing class body's indent. TS/JS class bodies use brace-delimited scope with conventional indent. Rust impl blocks and Go struct fields also have language-idiomatic indentation. An AST probe of all 6 language grammars confirms the node structures:
- **Python**: `class_definition` → `block` (children are methods/fields, all indented)
- **TypeScript/JS**: `class_declaration` → `class_body` (delimited by `{` `}`, children are `method_definition`/`public_field_definition`)
- **Rust**: `impl_item` → `declaration_list` (delimited by `{` `}`, children are `function_item`)
- **Rust structs**: `struct_item` → `field_declaration_list` (children are `field_declaration`)
- **Go**: `type_declaration` → `type_spec` → `struct_type` → `field_declaration_list` (children are `field_declaration`)

For compound operations, the AST probe reveals:
- **Rust `add_derive`**: `attribute_item` is a **sibling** (not child) of `struct_item`/`enum_item`. The derive arguments live in `attribute` → `token_tree` as `identifier` nodes. Appending a derive means either modifying the existing `token_tree` text or inserting a new `attribute_item` before the struct.
- **Python `add_decorator`**: `decorated_definition` wraps `function_definition`/`class_definition`. Decorators are `decorator` children of `decorated_definition`. Adding a decorator means inserting a `@name` line before the function def with matching indentation.
- **TS/JS `wrap_try_catch`**: `function_declaration` has a `statement_block` body. Wrapping means re-indenting the body content and wrapping in `try { ... } catch (error) { throw error; }`.
- **Go `add_struct_tags`**: `field_declaration` has an optional `raw_string_literal` as the last child for struct tags. Adding/modifying tags means editing or appending this literal.

Primary recommendation: build `indent.rs` (shared utility) first, then `add_member` command (proves indentation works), then the four compound operations in web-first order.

## Recommendation

**Task ordering:**

1. **Shared indentation utility (`src/indent.rs`)** — `detect_indent(source) -> IndentStyle` analyzes a file's indentation style (tabs vs spaces, width) from existing content lines. `indent_string(style) -> &str` returns the appropriate whitespace. Used by `add_member` and all compound operations.

2. **`add_member` command** — Scope-aware insertion into classes/structs/impl blocks. For each language: parse AST to find the target scope container, detect the body's indentation from existing children, determine insertion point (position param: `first`, `last`, `before:name`, `after:name`), indent the provided code to match, insert at the correct byte offset, write + validate. This is the core R014 deliverable.

3. **Compound operations** — Four language-specific commands, each self-contained. Web-first order: `wrap_try_catch` (TS/JS), `add_decorator` (Python), `add_derive` (Rust), `add_struct_tags` (Go). Each follows the same handler pattern but with different AST navigation logic.

4. **Plugin tool registrations** — Register all 5 commands in the OpenCode plugin with Zod schemas. One new file: `opencode-plugin-aft/src/tools/structure.ts`.

**Why this order:** Indentation detection is a prerequisite for everything else. `add_member` is the highest-value and hardest operation (R014 is core-capability). Compound operations compose on patterns established by `add_member`. Plugin registration is mechanical and done last.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Tree-sitter AST parsing | `tree-sitter` crate + grammars (already embedded) | All scope containers exposed as typed nodes — `class_body`, `block`, `declaration_list`, `field_declaration_list`. No additional parsing needed. |
| Byte-range editing | `edit::replace_byte_range()` in `src/edit.rs` | Already battle-tested by S01 and M001 commands. Handles insert-at-position via `replace_byte_range(source, pos, pos, new_text)`. |
| File backup before mutation | `edit::auto_backup()` in `src/edit.rs` | Standard mutation safety. All mutation commands must call this before `fs::write`. |
| Syntax validation | `edit::validate_syntax()` in `src/edit.rs` | Post-mutation syntax check. Returns `Some(true/false)` or `None` for unsupported languages. |
| Import/command handler pattern | `src/commands/add_import.rs` | Same structure: extract params → validate → parse AST → find target → backup → mutate → write → validate → respond. |
| Language detection | `parser::detect_language()` | Maps file extension to `LangId`. Already pub, already handles all 6 languages. |
| Grammar access | `parser::grammar_for()` | Returns tree-sitter Language grammar. Made pub in S01 (D051). |

## Existing Code and Patterns

- `src/edit.rs` — `replace_byte_range()`, `auto_backup()`, `validate_syntax()`, `line_col_to_byte()`. All insertion/mutation utilities needed by S02. Insert-at-position is `replace_byte_range(source, byte_pos, byte_pos, new_text)`.
- `src/commands/add_import.rs` — Reference implementation for the command handler pattern with AppContext. Follow this structure for `add_member` and compound operation handlers.
- `src/parser.rs` — `FileParser::parse()` returns `(&Tree, LangId)`. The `node_text()` and `node_range()` utilities are private — they'll be needed in `add_member` scope detection. Consider making them `pub(crate)` or extracting the scope-finding logic into a shared module.
- `src/parser.rs::py_scope_chain()` — Walks parent nodes to build scope chains. Similar parent-walking pattern will be needed for `add_member` scope resolution. However, `add_member` needs to find the scope container node itself (not just the chain of names).
- `src/imports.rs` — The `parse_file_imports()` convenience function wraps creating a `Parser`, parsing, and walking the AST. S02's `add_member` needs a similar convenience for finding scope containers. Consider a shared `parse_file()` utility that returns `(Tree, source, LangId)`.
- `src/context.rs` — `AppContext` with `provider()`, `backup()`, `config()`. No changes needed for S02.
- `src/symbols.rs` — `SymbolKind` enum defines Class, Struct, Method, Function, etc. `add_member` disambiguation will return structured results using these types.
- `src/commands/mod.rs` — Module registry. Add `add_member`, `add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags`.
- `src/main.rs` — Dispatch table. Add 5 new command match arms.
- `opencode-plugin-aft/src/tools/imports.ts` — Plugin tool registration pattern. S02 creates a new file `structure.ts` for the 5 new tools.

## Constraints

- **Single-threaded binary** — RefCell interior mutability (D014/D029). All file operations are synchronous. No async.
- **No protocol changes** — New commands use existing NDJSON envelope. New command strings + params fields.
- **Web-first language priority** (D004) — TS/JS/TSX → Python → Rust → Go for each feature.
- **Handler signature** — `handle_*(req: &RawRequest, ctx: &AppContext) -> Response` (D026).
- **Plugin Zod re-export** (D034) — `const z = tool.schema;` not direct `import { z } from "zod"`.
- **`node_text()` and `node_range()` are private in parser.rs** — Either make them `pub(crate)` or duplicate the logic. Making them `pub(crate)` is cleaner and follows the DRY principle.
- **Indentation detection must be shared** (D042) — Not per-command. Single `src/indent.rs` utility.

## Common Pitfalls

- **Python indentation is scope** — Inserting a method into a Python class at the wrong indent level changes semantics. A 4-space-indented method is a class method; a 0-space-indented `def` below the class is a module-level function. Solution: detect indent from the *first existing child* of the `block` node inside `class_definition`. If the block has children, use their indent. If the block is empty, use `detect_indent(source)` default + 1 level.
- **Rust attribute_item is a sibling, not a child** — `#[derive(Debug)]` and `pub struct Foo` are siblings under `source_file`. Walking `struct_item.parent().children()` to find preceding attributes. Must walk backward from the struct node to find consecutive `attribute_item` siblings.
- **Rust impl block type resolution** — `impl MyStruct { ... }` vs `impl Trait for MyStruct { ... }` have different scope semantics. The `add_member` `scope` param must handle both forms. Tree-sitter exposes both as `impl_item` with `type_identifier` children — distinguish by counting type identifiers or checking for the `for` keyword.
- **Go struct tags are backtick-delimited raw strings** — `field_declaration` → `raw_string_literal` is the existing tag. Adding tags to a field that already has tags means parsing the existing tag string and adding/updating key-value pairs. Adding to a field without tags means appending a `raw_string_literal` after the type.
- **TS class body children include semicolons and braces** — When walking `class_body` children to find the last method, skip `{`, `}`, and `;` nodes. Named children are more reliable than positional children.
- **`wrap_try_catch` must preserve indentation** — The function body's existing indentation must be increased by one level inside the try block. Each line needs re-indenting. Empty lines should not get trailing whitespace.
- **Empty scope containers** — A class/struct/impl with no existing members. `add_member` must handle insertion into empty bodies. For Python, insert after the `:` with one level of indent. For brace-delimited languages, insert between `{` and `}` with appropriate indent.
- **`add_decorator` on already-decorated functions** — If a function already has a `decorated_definition` wrapper, the new decorator should be inserted as the first/last decorator line, not re-wrapping the whole definition.

## Open Risks

- **Indentation detection edge cases** — Files mixing tabs and spaces, files with no existing indentation to detect (empty files or top-level-only code). Mitigation: default to 4 spaces for Python, 2 spaces for TS/JS/TSX, 4 spaces for Rust, and tabs for Go. Only override with detected style if confidence is high (>50% of indented lines agree).
- **Position parameter semantics for `add_member`** — `after:methodName` requires finding the named member in the scope. If the named member doesn't exist, should it error or fall back to `last`? Recommend: error with `member_not_found` code so the agent can retry with a different position.
- **Go struct tag syntax parsing** — Struct tags have a mini-syntax inside backtick strings: `json:"name" xml:"name,omitempty"`. Parsing this correctly for `add_struct_tags` requires understanding the space-separated key:"value" format. Risk of edge cases with nested quotes. Mitigation: treat the tag value as opaque where possible; for adding new keys, append `key:"value"` with proper spacing.
- **`wrap_try_catch` for arrow functions** — Arrow functions without braces (`const f = (x) => x + 1;`) need wrapping too. The body is an expression, not a statement block. Need to convert to `{ try { return expr; } catch ... }`. This adds a `return` statement that changes semantics for void functions. Consider limiting `wrap_try_catch` to functions with `statement_block` bodies initially.

## AST Node Types Reference

Critical reference for implementation — verified by tree-sitter AST probe:

### Scope Containers (for `add_member`)

| Language | Scope Node | Body Node | Body Children |
|----------|-----------|-----------|---------------|
| TS/JS | `class_declaration` | `class_body` | `method_definition`, `public_field_definition` |
| Python | `class_definition` | `block` | `function_definition`, `expression_statement`, `decorated_definition` |
| Rust (impl) | `impl_item` | `declaration_list` | `function_item` |
| Rust (struct) | `struct_item` | `field_declaration_list` | `field_declaration` |
| Go (struct) | `type_declaration` → `struct_type` | `field_declaration_list` | `field_declaration` |

### Compound Operation Targets

| Operation | Language | Target Node | Modification |
|-----------|----------|-------------|--------------|
| `add_derive` | Rust | `struct_item` or `enum_item` | Find preceding sibling `attribute_item` with `derive` identifier. Append to `token_tree` if exists, insert new `attribute_item` if not. |
| `wrap_try_catch` | TS/JS | `function_declaration` or `method_definition` | Replace `statement_block` content with `try { ...body... } catch (error) { throw error; }` |
| `add_decorator` | Python | `function_definition` or `class_definition` | Insert `@decorator_name` line before the def, with matching indentation. Handle already-decorated (has `decorated_definition` parent). |
| `add_struct_tags` | Go | `field_declaration` in `struct_type` | Add or update `raw_string_literal` child with tag key-value pair. |

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| tree-sitter | plurigrid/asi@tree-sitter | available (7 installs — too low adoption, not relevant) |
| tree-sitter | ssiumha/dots@tree-sitter | available (3 installs — not relevant) |
| Rust | github/awesome-copilot@rust-mcp-server-generator | available (7K installs — MCP server generation, not AST manipulation) |
| Rust | wshobson/agents@rust-async-patterns | available (4K installs — async patterns, binary is single-threaded) |

No skills are directly relevant. The work is domain-specific tree-sitter AST manipulation that doesn't map to general-purpose skills.

## Sources

- Tree-sitter AST node types verified by building and running an example binary against all 6 grammars — confirmed `class_body`, `block`, `declaration_list`, `field_declaration_list`, `attribute_item`, `decorated_definition`, `statement_block`, `raw_string_literal` node structures (source: local AST probe experiment)
- S01 summary provides forward intelligence on command handler pattern, plugin tool registration, and import engine architecture (source: `.gsd/milestones/M002/slices/S01/S01-SUMMARY.md`)
- Existing codebase — `parser.rs` (1960 lines), `imports.rs` (1662 lines), `edit.rs` (174 lines), `commands/edit_symbol.rs` (262 lines) — read directly to understand patterns and reusable utilities
- Indentation conventions: Python PEP 8 (4 spaces), TS/JS (2 spaces common), Rust (4 spaces), Go (tabs) — standard language conventions used as defaults
