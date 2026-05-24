# S02: Scope-aware Insertion & Compound Operations — UAT

**Milestone:** M002
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: All commands are exercised through the binary protocol via AftProcess integration tests and plugin round-trip tests. No live runtime, UI, or human judgment needed.

## Preconditions

- `cargo build` succeeds with 0 warnings
- `cargo test` passes all existing tests (no regressions)
- `bun install` completed in `opencode-plugin-aft/`
- Binary at `target/debug/aft` is current

## Smoke Test

Run `cargo test -- member` and `cargo test -- structure` — if all 35 tests pass, the slice basically works.

## Test Cases

### 1. add_member: Insert method at end of TS class

1. Send `add_member` with `file: member_ts.ts`, `scope: UserService`, `code: "  logout() { return true; }"`, `position: last`
2. Read the modified file
3. **Expected:** New method appears as the last member inside `UserService` class body, indented at 2 spaces matching existing members. Response includes `syntax_valid: true` and `backup_id`.

### 2. add_member: Insert method at beginning of TS class

1. Send `add_member` with `file: member_ts.ts`, `scope: UserService`, `position: first`, `code: "  init() {}"`
2. **Expected:** New method appears as the first member inside `UserService`, before existing methods.

### 3. add_member: Insert after specific member

1. Send `add_member` with `file: member_ts.ts`, `scope: UserService`, `position: after:getName`, `code: "  setName(n: string) { this.name = n; }"`
2. **Expected:** New method appears immediately after `getName` method. Response includes `syntax_valid: true`.

### 4. add_member: Python class preserves 4-space indentation

1. Send `add_member` with `file: member_py.py`, `scope: Calculator`, `code: "def multiply(self, a, b):\n    return a * b"`, `position: last`
2. Read modified file
3. **Expected:** New method appears with 4-space indentation matching existing methods (`    def multiply...` with `        return ...`).

### 5. add_member: Rust impl block method insertion

1. Send `add_member` with `file: member_rs.rs`, `scope: Config`, `code: "    pub fn reset(&mut self) {\n        self.debug = false;\n    }"`, `position: last`
2. **Expected:** Method inserted into `impl Config` block (not `struct Config`). Response includes `syntax_valid: true`.

### 6. add_member: Rust struct field insertion (empty struct)

1. Send `add_member` with `file: member_rs.rs`, `scope: EmptyStruct`, `code: "    count: u32,"`, `position: last`
2. **Expected:** Field inserted into `EmptyStruct` struct body (no impl block exists for this name).

### 7. add_member: Go struct field insertion

1. Send `add_member` with `file: member_go.go`, `scope: Server`, `code: "\tPort int"`, `position: last`
2. **Expected:** Field inserted into `Server` struct with tab indentation matching Go convention.

### 8. add_member: Empty class body

1. Send `add_member` with `file: member_ts.ts`, `scope: EmptyClass`, `code: "  hello() {}"`, `position: last`
2. **Expected:** Method inserted into the empty class body. File remains syntactically valid.

### 9. add_derive: Append to existing derive attribute

1. Send `add_derive` with `file: structure_rs.rs`, `target: Point`, `derives: ["Clone"]` where Point already has `#[derive(Debug)]`
2. **Expected:** Existing attribute becomes `#[derive(Debug, Clone)]`. Response includes `derives: ["Debug", "Clone"]` and `syntax_valid: true`.

### 10. add_derive: Create new derive on undecorated struct

1. Send `add_derive` with `file: structure_rs.rs`, `target: Config`, `derives: ["Serialize"]` where Config has no derive
2. **Expected:** New `#[derive(Serialize)]` line inserted before `struct Config`. Response includes `derives: ["Serialize"]`.

### 11. add_derive: Deduplication

1. Send `add_derive` with `file: structure_rs.rs`, `target: Point`, `derives: ["Debug"]` where Point already has `#[derive(Debug)]`
2. **Expected:** No duplicate — derive list stays `["Debug"]`. File unchanged.

### 12. wrap_try_catch: Simple function

1. Send `wrap_try_catch` with `file: structure_ts.ts`, `target: processData`
2. Read modified file
3. **Expected:** Function body wrapped in `try { ... } catch (error) { throw error; }`. Body re-indented +1 level. `syntax_valid: true`.

### 13. wrap_try_catch: Class method

1. Send `wrap_try_catch` with `file: structure_ts.ts`, `target: handleRequest`
2. **Expected:** Class method body wrapped in try/catch. Indentation correct within class context.

### 14. wrap_try_catch: Custom catch body

1. Send `wrap_try_catch` with `file: structure_ts.ts`, `target: processData`, `catch_body: "console.error(error);"`
2. **Expected:** Catch block contains `console.error(error);` instead of default `throw error;`.

### 15. add_decorator: Plain function

1. Send `add_decorator` with `file: structure_py.py`, `target: process_data`, `decorator: "cache_result"`
2. Read modified file
3. **Expected:** `@cache_result` line inserted before `def process_data`, matching the function's indentation.

### 16. add_decorator: Already-decorated function (last position)

1. Send `add_decorator` with `file: structure_py.py`, `target: helper`, `decorator: "timer"`, `position: last` where helper already has `@staticmethod`
2. **Expected:** `@timer` inserted after existing `@staticmethod` decorator, immediately before `def helper`.

### 17. add_decorator: Already-decorated function (first position)

1. Send `add_decorator` with `file: structure_py.py`, `target: helper`, `decorator: "timer"`, `position: first`
2. **Expected:** `@timer` inserted before all existing decorators.

### 18. add_struct_tags: Add tag to untagged field

1. Send `add_struct_tags` with `file: structure_go.go`, `struct: Server`, `field: Host`, `key: json`, `value: host`
2. Read modified file
3. **Expected:** Field has `` `json:"host"` `` appended after type. Response includes `tag_string`.

### 19. add_struct_tags: Add tag to field with existing tags

1. Send `add_struct_tags` with `file: structure_go.go`, `struct: Server`, `field: Port`, `key: yaml`, `value: port` where Port already has `json:"port"`
2. **Expected:** Tag becomes `` `json:"port" yaml:"port"` ``. Both tags present.

### 20. add_struct_tags: Update existing tag value

1. Send `add_struct_tags` with `file: structure_go.go`, `struct: Server`, `field: Port`, `key: json`, `value: server_port` where Port has `json:"port"`
2. **Expected:** Tag value updated to `json:"server_port"`. Response includes updated `tag_string`.

### 21. Plugin registration: All 5 tools discoverable

1. Run `bun test` in `opencode-plugin-aft/`
2. **Expected:** All 5 structure tools (add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags) registered with correct descriptions and Zod schemas. 36 total tests pass.

## Edge Cases

### scope_not_found error

1. Send `add_member` with a non-existent scope name
2. **Expected:** Error response with `code: "scope_not_found"`, message includes the scope name searched and list of available scopes in the file.

### member_not_found error

1. Send `add_member` with `position: after:nonexistent`
2. **Expected:** Error response with `code: "member_not_found"`, message includes the member name and scope.

### target_not_found error (compound ops)

1. Send `add_derive` with a non-existent struct name
2. **Expected:** Error response with `code: "target_not_found"`, message includes available struct/enum names.

### field_not_found error (add_struct_tags)

1. Send `add_struct_tags` with a valid struct but non-existent field name
2. **Expected:** Error response with `code: "field_not_found"`, message includes available field names in the struct.

### wrap_try_catch on missing function

1. Send `wrap_try_catch` with a function name that doesn't exist in the file
2. **Expected:** Error response with `code: "target_not_found"`, message includes available function names.

### missing required params

1. Send `add_member` without `scope` or `code` params
2. **Expected:** Error response with `code: "invalid_request"`, descriptive message about missing params.

## Failure Signals

- Any `cargo test -- member` or `cargo test -- structure` failure
- Any `bun test` failure in `opencode-plugin-aft/`
- `cargo build` producing warnings
- Error responses missing structured `code` field
- Indentation mismatch in inserted code (Python 4-space is the critical case)
- Rust add_member inserting into struct body instead of impl body when both exist

## Requirements Proved By This UAT

- R014 — add_member inserts methods/fields into TS/JS classes, Python classes, Rust impl blocks/structs, and Go structs with correct indentation and 4 position modes
- R015 — add_derive, wrap_try_catch, add_decorator, add_struct_tags all perform language-specific structural transforms through the binary protocol

## Not Proven By This UAT

- Auto-format after insertion (R016 — S03 scope)
- Dry-run preview of member insertion (R018 — S04 scope)
- Multi-file transactions involving member insertion (R019 — S04 scope)
- Cross-file scope resolution (single-file only)

## Notes for Tester

- All test cases are already automated as integration tests (`member_test.rs`, `structure_test.rs`, `structure.test.ts`). Manual re-testing is only needed if the integration tests pass but behavior seems wrong.
- Fixture files are in `tests/fixtures/` — `member_*.{ts,py,rs,go}` and `structure_*.{rs,ts,py,go}`.
- The Rust binary must be rebuilt (`cargo build`) before running plugin tests if any Rust source changed.
