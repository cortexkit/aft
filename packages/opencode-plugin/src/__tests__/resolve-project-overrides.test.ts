/// <reference path="../bun-test.d.ts" />

/**
 * Tests for `resolveProjectOverridesForConfigure` — the function that
 * extracts the per-project-overridable subset of an AftConfig to feed into
 * the BridgePool's `projectConfigLoader` callback.
 *
 * v0.27.1 motivation: in OpenCode Desktop / `opencode serve` mode, one
 * plugin instance serves many projects. Without per-project overrides, every
 * bridge inherits whatever project config was visible at plugin init time.
 * The user reported this with `experimental.bash.background: false` in
 * project A being ignored because plugin init loaded a different project.
 *
 * The function's contract (see config.ts doc-comment):
 *   - INCLUDES every field that can legitimately differ per project:
 *     format_on_edit, formatter_timeout_secs, validate_on_edit, formatter,
 *     checker, restrict_to_project_root, search_index, semantic_search,
 *     experimental.bash.*, experimental.lsp_ty, lsp (project-safe subset),
 *     semantic (project-safe subset), max_callgraph_files.
 *   - EXCLUDES tool-registration fields that lock at plugin init:
 *     tool_surface, disabled_tools, hoist_builtin_tools (OpenCode registers
 *     tools synchronously when the plugin function returns).
 *   - EXCLUDES global per-process state injected at plugin init:
 *     storage_dir, _ort_dylib_dir, harness, bash_permissions, lsp_paths_extra.
 *   - Always sets `restrict_to_project_root` (defaulting to false) so the
 *     Rust side doesn't fall back to its own historical default.
 */

import { describe, expect, test } from "bun:test";
import { resolveProjectOverridesForConfigure } from "../config.js";

describe("resolveProjectOverridesForConfigure", () => {
  test("empty config returns restrict_to_project_root default + graduated bash defaults", () => {
    // Rust expects restrict_to_project_root; we explicitly set false (parity
    // with OpenCode built-in tools) so it doesn't fall back to its own default.
    //
    // Post-v0.27.2 graduation: bash is on by default for the implicit
    // `recommended` tool_surface, so `resolveBashConfig` emits true for all
    // three sub-features. They flow through to Rust as flat keys.
    expect(resolveProjectOverridesForConfigure({})).toEqual({
      restrict_to_project_root: false,
      experimental_bash_rewrite: true,
      experimental_bash_compress: true,
      experimental_bash_background: true,
    });
  });

  test("includes every per-project-overridable field when set", () => {
    const overrides = resolveProjectOverridesForConfigure({
      format_on_edit: true,
      formatter_timeout_secs: 30,
      validate_on_edit: "syntax",
      formatter: { typescript: "biome" },
      checker: { typescript: "biome" },
      restrict_to_project_root: true,
      search_index: true,
      semantic_search: true,
      experimental: {
        bash: { rewrite: true, compress: true, background: false },
        lsp_ty: true,
      },
      semantic: { backend: "fastembed", timeout_ms: 25000 },
      max_callgraph_files: 10000,
    });

    expect(overrides).toEqual({
      format_on_edit: true,
      formatter_timeout_secs: 30,
      validate_on_edit: "syntax",
      formatter: { typescript: "biome" },
      checker: { typescript: "biome" },
      restrict_to_project_root: true,
      search_index: true,
      semantic_search: true,
      experimental_bash_rewrite: true,
      experimental_bash_compress: true,
      experimental_bash_background: false,
      experimental_lsp_ty: true,
      semantic: { backend: "fastembed", timeout_ms: 25000 },
      max_callgraph_files: 10000,
    });
  });

  test("project-level bash.background:false flows through (v0.27.1 regression)", () => {
    // Exact user scenario: user has bash.background:true (globally enabled),
    // project A sets bash.background:false (opt out for this project). Before
    // the fix, project A's override never reached the bridge in Desktop mode.
    // After the fix, mergeConfigs(user, project) produces this shape and
    // resolveProjectOverridesForConfigure flattens it to the Rust wire format.
    const merged = {
      experimental: {
        bash: {
          rewrite: true, // inherited from user
          compress: true, // inherited from user
          background: false, // project override
        },
      },
    };
    const overrides = resolveProjectOverridesForConfigure(merged);

    expect(overrides.experimental_bash_background).toBe(false);
    expect(overrides.experimental_bash_rewrite).toBe(true);
    expect(overrides.experimental_bash_compress).toBe(true);
  });

  test("omits undefined fields (so global overrides shine through on shallow merge)", () => {
    // The pool does `{ ...global, ...projectOverrides }`. Any undefined value
    // here would clobber the global value. Excluding them keeps the merge
    // semantically clean.
    //
    // Post-v0.27.2: bash defaults flow through unconditionally because the
    // resolver materializes the surface default. Other unspecified fields
    // are still omitted as before.
    const overrides = resolveProjectOverridesForConfigure({
      format_on_edit: true,
      // formatter_timeout_secs and validate_on_edit left undefined
    });

    expect(overrides).toEqual({
      format_on_edit: true,
      restrict_to_project_root: false, // always set
      // Graduated bash defaults are always materialized (see resolveBashConfig).
      experimental_bash_rewrite: true,
      experimental_bash_compress: true,
      experimental_bash_background: true,
    });
    expect("formatter_timeout_secs" in overrides).toBe(false);
    expect("validate_on_edit" in overrides).toBe(false);
  });

  test("EXCLUDES tool-registration fields that lock at plugin init", () => {
    // tool_surface, disabled_tools, and hoist_builtin_tools affect which
    // tools OpenCode registers when the plugin function returns. They
    // CANNOT change per-bridge. Smuggling them through the per-project
    // override path would silently fail to take effect — worse than
    // documenting the limitation. See note #157 for the v0.28+ design.
    const overrides = resolveProjectOverridesForConfigure({
      tool_surface: "minimal",
      disabled_tools: ["aft_navigate"],
      hoist_builtin_tools: false,
      // One real per-project field to confirm the function still works.
      format_on_edit: true,
    });

    expect("tool_surface" in overrides).toBe(false);
    expect("disabled_tools" in overrides).toBe(false);
    expect("hoist_builtin_tools" in overrides).toBe(false);
    expect(overrides.format_on_edit).toBe(true);
  });

  test("EXCLUDES global per-process state keys (defensive guard)", () => {
    // These fields aren't AftConfig schema fields — they're set at plugin
    // init from process state (XDG dirs, ONNX download path, harness ID,
    // LSP install cache). A future schema change could accidentally surface
    // them; this test catches that.
    const overrides = resolveProjectOverridesForConfigure({});
    const forbiddenGlobals = [
      "storage_dir",
      "_ort_dylib_dir",
      "harness",
      "bash_permissions",
      "lsp_paths_extra",
      "lsp_inflight_installs",
    ];
    for (const key of forbiddenGlobals) {
      expect(key in overrides).toBe(false);
    }
  });
});
