---
estimated_steps: 4
estimated_files: 3
---

# T03: Plugin tool registrations for all 5 commands

**Slice:** S02 — Scope-aware Insertion & Compound Operations
**Milestone:** M002

## Description

Register all 5 S02 commands (`add_member`, `add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags`) as OpenCode plugin tools with Zod schemas. This completes the integration surface so agents can access S02 commands through the plugin. Follows the established pattern from `imports.ts` (D034).

## Steps

1. **Create `opencode-plugin-aft/src/tools/structure.ts`** — Export `structureTools(bridge: BinaryBridge)` returning `Record<string, ToolDefinition>`. Define Zod schemas for all 5 commands using `const z = tool.schema` (D034). `add_member`: file, scope, code, position (optional enum). `add_derive`: file, target, derives (string array). `wrap_try_catch`: file, target, catch_body (optional). `add_decorator`: file, target, decorator, position (optional enum). `add_struct_tags`: file, target, field, tag, value. Each tool's execute function builds params and calls `bridge.send()`.

2. **Wire into `opencode-plugin-aft/src/index.ts`** — Import `structureTools` from `./tools/structure.js`. Spread into the tool object. Update the JSDoc comment listing tool categories to include "Structure: add_member, add_derive, wrap_try_catch, add_decorator, add_struct_tags".

3. **Write bun tests** — `opencode-plugin-aft/src/__tests__/structure.test.ts` verifying all 5 tools are registered with correct schemas and can be called through the bridge (following the pattern in existing test files). Test that required params are enforced by schemas and optional params work.

4. **Final verification** — Run `bun test` to confirm all existing + new tests pass. Run `cargo test` to confirm no regressions. Verify `cargo build` produces 0 warnings.

## Must-Haves

- [ ] All 5 commands registered as tools with descriptive descriptions
- [ ] Zod schemas enforce required params and correctly type optional params
- [ ] `structureTools` wired into plugin index and tools are discoverable
- [ ] Bun tests pass for all 5 tool registrations
- [ ] No regressions in existing plugin tests

## Verification

- `cd opencode-plugin-aft && bun test` — all tests pass including new structure tool tests
- `cargo test` — no regressions (final full run)
- `cargo build 2>&1 | grep -c warning` → 0

## Inputs

- `opencode-plugin-aft/src/tools/imports.ts` — pattern reference for tool registration
- `opencode-plugin-aft/src/__tests__/tools.test.ts` — pattern reference for tests
- `opencode-plugin-aft/src/index.ts` — wiring target
- T01/T02 completed: all 5 commands working in the binary

## Observability Impact

- **No new runtime signals.** The plugin layer is a thin pass-through — all stderr logging (`[aft] add_member: {file}`, etc.) and structured error codes (`scope_not_found`, `target_not_found`, `invalid_request`) come from the binary, which was instrumented in T01/T02.
- **Discoverability signal:** After this task, the 5 structure tools appear in the OpenCode plugin's tool registry. An agent can confirm registration by listing available tools — `add_member`, `add_derive`, `wrap_try_catch`, `add_decorator`, `add_struct_tags` should all be present.
- **Failure visibility:** Schema validation errors (missing required params) are surfaced by Zod before reaching the binary. Binary-level errors (`scope_not_found` with available scopes, `target_not_found` with available targets) pass through as JSON in the tool return value.

## Expected Output

- `opencode-plugin-aft/src/tools/structure.ts` — 5 tool definitions (~150 lines)
- `opencode-plugin-aft/src/index.ts` — updated with structureTools import and spread
- `opencode-plugin-aft/src/__tests__/structure.test.ts` — registration and schema tests (~80 lines)
