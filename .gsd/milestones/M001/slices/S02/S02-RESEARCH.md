# S02: Tree-sitter Multi-Language Engine — Research

**Date:** 2026-03-14

## Summary

S02 implements the core tree-sitter parsing and symbol extraction engine for 6 languages (TS, JS, TSX, Python, Rust, Go). The Rust `tree-sitter` crate (v0.26.6) provides a mature API: `Parser` + `Language` + `Query` + `QueryCursor`. Each grammar crate compiles its C parser at build time via `cc` and exposes a `LANGUAGE` constant (or `LANGUAGE_TYPESCRIPT` / `LANGUAGE_TSX` for the TS crate). S-expression query patterns with `@capture` names extract symbols from parsed trees.

The approach is: one `.scm` query file per language group (TS/TSX share one, JS gets its own, then Python, Rust, Go), loaded at startup and compiled into `Query` objects. A `FileParser` struct owns parsers per language and caches parsed trees. A `TreeSitterProvider` implements the existing `LanguageProvider` trait from S01. The `Symbol` struct needs significant expansion from S01's skeleton — adding `signature`, `scope_chain`, `exported`, `parent`, and changing `kind` from bare `String` to an enum.

The key risk is query pattern accuracy across language constructs — arrow functions assigned to const (TS/JS), methods inside impl blocks (Rust), receiver methods (Go), and decorator/export detection all need distinct handling. The official `tags.scm` files from each grammar repo provide a solid starting point but must be extended to capture full ranges, signatures, and scope context.

## Recommendation

**Implement a `FileParser` + `TreeSitterProvider` architecture with embedded `.scm` query strings per language.** Don't use runtime-loaded query files — embed them as `include_str!()` constants for zero-dependency operation. Write custom symbol extraction queries inspired by (but not identical to) the grammar repos' `tags.scm` patterns, because we need richer captures (full definition node for range, parameter lists for signatures, parent nodes for scope chains, export wrappers for export status).

Start with TS/TSX (shared query + JS superset), then JS, Python, Rust, Go — matching the web-first priority (D004). Test each language against representative real-world code files as you go. The `LanguageProvider` trait from S01 needs minor evolution — `list_symbols` should return `Vec<Symbol>` with the expanded struct, `resolve_symbol` stays as-is.

Parse tree caching is important even in S02 — S03's outline/zoom and S05's edit_symbol will re-parse repeatedly. Use a simple `HashMap<PathBuf, (SystemTime, Tree)>` keyed on file path with mtime invalidation.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|-------------------|------------|
| Language parsing | `tree-sitter` crate (0.26.6) | Mature incremental parser, battle-tested in helix/zed/GitHub, handles all 6 target languages |
| TS/TSX grammar | `tree-sitter-typescript` crate (0.23.2) | Official grammar, exports both `LANGUAGE_TYPESCRIPT` and `LANGUAGE_TSX` |
| JS/JSX grammar | `tree-sitter-javascript` crate (0.25.0) | Official grammar, JSX built-in |
| Python grammar | `tree-sitter-python` crate (0.25.0) | Official grammar |
| Rust grammar | `tree-sitter-rust` crate (0.24.0) | Official grammar |
| Go grammar | `tree-sitter-go` crate (0.25.0) | Official grammar |
| Symbol extraction patterns | `tags.scm` from grammar repos | Starting point for query patterns — already tested against each grammar's test corpus |

## Existing Code and Patterns

- `src/language.rs` — defines `LanguageProvider` trait with `resolve_symbol()` and `list_symbols()`, plus `Symbol`, `SymbolMatch`, `Range` structs. S02 must implement this trait (replacing `StubProvider`) and expand the `Symbol` struct with `signature`, `scope_chain`, `exported`, `parent` fields.
- `src/error.rs` — `AftError::ParseError` variant ready for tree-sitter parse failures. `AftError::SymbolNotFound` and `AftError::AmbiguousSymbol` ready for resolution failures. No new error variants needed.
- `src/protocol.rs` — `RawRequest` with `params` map for per-command param extraction. New commands (if any in this slice) follow the `match` dispatch pattern in `main.rs`.
- `src/main.rs` — command dispatch via `match req.command.as_str()`. S02 likely doesn't add new commands (outline/zoom are S03), but wires the `TreeSitterProvider` into the process.
- `src/config.rs` — `Config` struct with `project_root` and `max_symbol_depth` — both relevant for symbol resolution. No changes needed.
- `tests/integration/protocol_test.rs` — `AftProcess` helper struct for spawning binary and sending JSON commands.

## Constraints

- **Grammar crates compile C code at build time** — requires `cc` (C compiler) on the build machine. Already available on this system (Apple clang 17.0.0). Cross-compilation in S07 must ensure C toolchains are available for all 5 targets.
- **tree-sitter-typescript 0.23.2 depends on `tree-sitter-language ^0.1`** while `tree-sitter` 0.26.6 also depends on `tree-sitter-language ^0.1`. These must resolve to the same version. The `LanguageFn` type bridges them — use `.into()` to convert `LanguageFn` to `Language`.
- **One parser per language at a time** — `Parser` is not `Send`/`Sync`. Since the binary is single-threaded (stdin loop), this is fine. Create parsers lazily and reuse.
- **Query patterns are language-specific** — can't share queries across language boundaries. A query compiled for TS grammar will panic if executed against a Python tree. Must validate language-query association at compile time or initialization.
- **Symbol struct must be `Serialize`** — downstream consumers (S03 outline, S05 edit_symbol) return symbols as JSON. Add `#[derive(Serialize)]` with serde.
- **`RawRequest.params` is `serde_json::Value`** — not a typed `Map<String, Value>`. Confirmed from code: `#[serde(flatten)] pub params: serde_json::Value` — this captures as a `Value::Object`.

## Common Pitfalls

- **TS/JS arrow functions assigned to variables** — `const foo = (x: number) => x + 1` must be detected as a function named `foo`. The tree-sitter AST is `lexical_declaration > variable_declarator > (name: identifier, value: arrow_function)`. The JS tags.scm handles this but it requires a 3-level pattern. Must include this in both TS and JS queries.
- **Rust methods vs functions** — in the Rust grammar, methods inside `impl` blocks are `function_item` nodes inside `declaration_list`. The only way to distinguish methods from free functions is checking parent context (is there an enclosing `impl_item`?). The tags.scm uses `(declaration_list (function_item ...))` as `definition.method`.
- **Go receiver methods** — `method_declaration` has a receiver parameter `(parameter_list ...)` before the method name. The name field is `field_identifier`, not `identifier`. Must capture with `(method_declaration name: (field_identifier) @name)`.
- **Export detection in TS/JS** — exports are wrapper nodes: `(export_statement declaration: ...)`. The symbol's declaration is a child of the export_statement. Must check parent or capture the export wrapper separately. For `export default`, the pattern is different: `(export_statement "default" value: ...)`.
- **Python scope chains** — Python uses indentation for scope. A method `def bar(self)` inside `class Foo:` has the parent relationship encoded in the tree structure (function_definition is a child of block → class_definition), not in syntax keywords. Must walk parent nodes to build scope chain.
- **TypeScript `type_identifier` vs `identifier`** — classes and interfaces use `type_identifier` for their name, while functions use `identifier`. Query patterns must use the correct node type or the capture will miss.
- **tree-sitter Query compilation errors are not great** — a typo in a .scm pattern gives an error offset and error type but no human-readable message. Test queries against real code immediately after writing them.
- **Tree caching with mtime** — `SystemTime` comparison across file systems can be unreliable. Use byte equality of mtime values rather than ordering.

## Open Risks

- **Version compatibility between `tree-sitter` 0.26.6 and grammar crates** — grammar crates target `tree-sitter-language ^0.1` which is a bridge crate. Need to verify all 5 grammar crates compile together without version conflicts. First `cargo build` after adding all dependencies will surface this.
- **TSX query patterns for JSX components** — a React component like `const Foo: React.FC = () => <div/>` is a variable declaration with an arrow function value AND JSX in the body. The TSX grammar must handle JSX without the query choking. The TS grammar handles this by using a separate TSX dialect — must use `LANGUAGE_TSX` for `.tsx` files.
- **Rust impl blocks with multiple trait impls** — `impl Display for Foo` and `impl Debug for Foo` both define methods on `Foo`. The scope chain for methods should include the impl target type AND the trait (if any). Need to capture `impl_item` → `type` and optionally `trait` fields.
- **Large file performance** — tree-sitter parsing is O(n) and very fast (~1ms for typical files), but query execution on very large files (10k+ lines) with many captures could be slower. Not a launch blocker but should measure.
- **`cc` crate build time** — compiling 5 C grammar parsers at build time adds significant compile time (each grammar is ~500KB of generated C). First build will be slow (~30-60s for grammars alone). Incremental rebuilds only recompile changed grammars.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| tree-sitter | `plurigrid/asi@tree-sitter` | available (7 installs — low adoption, likely not worth installing) |
| tree-sitter | `ssiumha/dots@tree-sitter` | available (3 installs — very low adoption) |
| Rust | none relevant found | none found |

No skills are worth recommending for installation. The tree-sitter skills have very low adoption and the tree-sitter Rust crate API is well-documented enough via `resolve_library`/`get_library_docs`.

## Sources

- Crate versions and API: `tree-sitter` 0.26.6 (source: [crates.io](https://crates.io/crates/tree-sitter)), uses `Parser::new()` + `parser.set_language(&LANGUAGE.into())` + `Query::new(language, pattern)` + `QueryCursor::matches()`
- TypeScript crate exports `LANGUAGE_TYPESCRIPT` and `LANGUAGE_TSX` constants (source: [docs.rs source](https://docs.rs/tree-sitter-typescript/0.23.2/src/tree_sitter_typescript/lib.rs.html))
- JavaScript crate includes JSX support built-in (source: [crates.io](https://crates.io/crates/tree-sitter-javascript))
- Symbol extraction query patterns per language (source: `tags.scm` files in each grammar repo — [TS](https://github.com/tree-sitter/tree-sitter-typescript/blob/master/queries/tags.scm), [JS](https://github.com/tree-sitter/tree-sitter-javascript/master/queries/tags.scm), [Python](https://github.com/tree-sitter/tree-sitter-python/master/queries/tags.scm), [Rust](https://github.com/tree-sitter/tree-sitter-rust/master/queries/tags.scm), [Go](https://github.com/tree-sitter/tree-sitter-go/master/queries/tags.scm))
- AST node types for TS: `function_declaration`, `class_declaration`, `abstract_class_declaration`, `interface_declaration`, `enum_declaration`, `method_definition`, `arrow_function`, `type_alias_declaration` (source: [node-types.json](https://github.com/tree-sitter/tree-sitter-typescript/blob/master/typescript/src/node-types.json))
- AST node types for Rust: `function_item`, `struct_item`, `enum_item`, `trait_item`, `impl_item`, `union_item` with methods as `function_item` inside `declaration_list` (source: helix textobjects.scm for [Rust](https://github.com/helix-editor/helix/blob/master/runtime/queries/rust/textobjects.scm))
- AST node types for Go: `function_declaration`, `method_declaration`, `type_spec` inside `type_declaration` (source: helix textobjects.scm for [Go](https://github.com/helix-editor/helix/blob/master/runtime/queries/go/textobjects.scm))
- AST node types for Python: `function_definition`, `class_definition` (source: helix textobjects.scm for [Python](https://github.com/helix-editor/helix/blob/master/runtime/queries/python/textobjects.scm))
- JS arrow function detection pattern: `(lexical_declaration (variable_declarator name: (identifier) @name value: (arrow_function)))` (source: [JS tags.scm](https://github.com/tree-sitter/tree-sitter-javascript/master/queries/tags.scm))
- Helix ECMAScript textobject patterns as reference for shared TS/JS patterns (source: [ecma textobjects.scm](https://github.com/helix-editor/helix/blob/master/runtime/queries/ecma/textobjects.scm))
