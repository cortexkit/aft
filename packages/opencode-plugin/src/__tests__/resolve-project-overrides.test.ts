/// <reference path="../bun-test.d.ts" />

/**
 * Tests for `resolveProjectOverridesForConfigure` — the function that
 * extracts the per-project-overridable subset of an AftConfig to feed into
 * the BridgePool's `projectConfigLoader` callback.
 *
 * v0.27.1 motivation: in OpenCode Desktop / `opencode serve` mode, one
 * plugin instance serves many projects. Without per-project overrides, every
 * bridge inherits whatever project config was visible at plugin init time.
 * The user reported this with `bash.background: false` in
 * project A being ignored because plugin init loaded a different project.
 *
 * The function's contract (see config.ts doc-comment):
 *   - INCLUDES every field that can legitimately differ per project:
 *     format_on_edit, formatter_timeout_secs, validate_on_edit, formatter,
 *     checker, restrict_to_project_root, indexes, callgraph_chunk_size,
 *     experimental.bash.*, experimental.lsp_ty, lsp (project-safe subset),
 *   - Always forwards the resolved `disabled_tools` and `indexes`; a config
 *     without a resolved disabled list is refused rather than defaulted.
 *   - EXCLUDES global per-process state injected at plugin init:
 *     storage_dir, _ort_dylib_dir, harness, bash_permissions, lsp_paths_extra.
 *   - Always sets `restrict_to_project_root` (defaulting to false) so the
 *     Rust side doesn't fall back to its own historical default.
 */

import { describe, expect, test } from "bun:test";
import { resolveProjectOverridesForConfigure } from "../config.js";

/** Resolved fields `loadAftConfig` always provides. */
const RESOLVED = { disabled_tools: ["aft_delete", "aft_move"] };
const RESOLVED_OUT = {
  disabled_tools: ["aft_delete", "aft_move"],
  indexes: { callgraph: true, semantic: true, trigram: true },
};

describe("resolveProjectOverridesForConfigure", () => {
  test("empty config returns restrict_to_project_root default + graduated bash defaults", () => {
    // Rust expects restrict_to_project_root; we explicitly set false (parity
    // with OpenCode built-in tools) so it doesn't fall back to its own default.
    //
    // Post-v0.27.2 graduation: bash is on by default for the implicit
    // `recommended` tool_surface, so `resolveBashConfig` emits true for all
    // three sub-features. They flow through to Rust as flat keys.
    expect(resolveProjectOverridesForConfigure(RESOLVED)).toEqual({
      ...RESOLVED_OUT,
      restrict_to_project_root: false,
      experimental_bash_rewrite: true,
      experimental_bash_compress: true,
      experimental_bash_background: true,
    });
  });

  test("includes every per-project-overridable field when set", () => {
    const overrides = resolveProjectOverridesForConfigure({
      ...RESOLVED,
      format_on_edit: true,
      formatter_timeout_secs: 30,
      validate_on_edit: "syntax",
      formatter: { typescript: "biome" },
      checker: { typescript: "biome" },
      restrict_to_project_root: true,
      indexes: { trigram: true, semantic: true, callgraph: false },
      callgraph_chunk_size: 3,
      github: { shim: true, read: false, write: true },
      experimental: {
        bash: { rewrite: true, compress: true, background: false },
        lsp_ty: true,
      },
      semantic: { backend: "fastembed", timeout_ms: 25000 },
    });

    expect(overrides).toEqual({
      disabled_tools: ["aft_delete", "aft_move"],
      format_on_edit: true,
      formatter_timeout_secs: 30,
      validate_on_edit: "syntax",
      formatter: { typescript: "biome" },
      checker: { typescript: "biome" },
      restrict_to_project_root: true,
      indexes: { trigram: true, semantic: true, callgraph: false },
      callgraph_chunk_size: 3,
      github: { shim: true, read: true, write: true },
      experimental_bash_rewrite: true,
      experimental_bash_compress: true,
      experimental_bash_background: false,
      experimental_lsp_ty: true,
      semantic: { backend: "fastembed", timeout_ms: 25000 },
    });
  });

  test("forwards the project-settable host fallback gate as inert bash config", () => {
    const overrides = resolveProjectOverridesForConfigure({
      ...RESOLVED,
      bash: { host_fallback: true },
    });

    expect(overrides.bash).toEqual({ host_fallback: true });
  });

  test("project-level bash.background:false flows through (v0.27.1 regression)", () => {
    // Exact user scenario: user has bash.background:true (globally enabled),
    // project A sets bash.background:false (opt out for this project). Before
    // the fix, project A's override never reached the bridge in Desktop mode.
    // After the fix, mergeConfigs(user, project) produces this shape and
    // resolveProjectOverridesForConfigure flattens it to the Rust wire format.
    const merged = {
      ...RESOLVED,
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
      ...RESOLVED,
      format_on_edit: true,
      // formatter_timeout_secs and validate_on_edit left undefined
    });

    expect(overrides).toEqual({
      ...RESOLVED_OUT,
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

  test("forwards the resolved disabled list (including []) and refuses an absent one", () => {
    expect(
      resolveProjectOverridesForConfigure({ disabled_tools: ["aft_callgraph"] }).disabled_tools,
    ).toEqual(["aft_callgraph"]);
    expect(resolveProjectOverridesForConfigure({ disabled_tools: [] }).disabled_tools).toEqual([]);
    expect(() => resolveProjectOverridesForConfigure({})).toThrow(
      "invalid_resolved_config:missing:disabled_tools",
    );
  });

  test("EXCLUDES global per-process state keys (defensive guard)", () => {
    // These fields aren't AftConfig schema fields — they're set at plugin
    // init from process state (XDG dirs, ONNX download path, harness ID,
    // LSP install cache). A future schema change could accidentally surface
    // them; this test catches that.
    const overrides = resolveProjectOverridesForConfigure(RESOLVED);
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
