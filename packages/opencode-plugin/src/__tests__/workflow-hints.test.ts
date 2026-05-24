/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import type { AftConfig } from "../config.js";
import { buildHintsFromConfig, buildWorkflowHints } from "../workflow-hints.js";

describe("buildWorkflowHints", () => {
  test("renders all four sections at tool_surface=all with bg + semantic enabled", () => {
    const out = buildWorkflowHints({
      toolSurface: "all",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: true,
      disabledTools: new Set(),
    });
    expect(out).not.toBeNull();
    expect(out).toContain("## Prefer AFT tools for token efficiency");
    expect(out).toContain("**Web/URL access**");
    expect(out).toContain("**Code exploration**");
    expect(out).toContain("For exact identifiers (`useState`, function names, env vars)");
    expect(out).toContain(
      "For broad concepts ('where is X handled', 'how does Y work') → `aft_search`",
    );
    expect(out).toContain("Use `aft_navigate`");
    expect(out).toContain("- `callers`");
    expect(out).toContain("- `impact`");
    expect(out).toContain("- `trace_to`");
    expect(out).toContain("- `trace_data`");
    expect(out).toContain("**Long-running commands**");
    expect(out).toContain("`bash({ background: true })`");
    // Anti-polling guidance must be present so agents stop calling
    // bash_status back-to-back when waiting for a background task.
    expect(out).toContain("A completion reminder is delivered automatically");
    expect(out).toContain("do not poll");
  });

  test("omits long-running bash hint when background bash is off (foreground auto-promotes)", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      disabledTools: new Set(),
    });
    expect(out).not.toBeNull();
    // Foreground bash now auto-promotes after a short wait-window, so we
    // don't need to teach the agent about timeouts up front. The bash hint
    // section is gone entirely when bg-bash is disabled.
    expect(out).not.toContain("background: true");
    expect(out).not.toContain("**Long-running commands**");
    expect(out).not.toContain("**Long-running bash commands**");
    expect(out).not.toContain("30 seconds");
  });

  test("omits the navigate section at tool_surface=recommended", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      disabledTools: new Set(),
    });
    expect(out).not.toContain("Use `aft_navigate`");
    expect(out).not.toContain("- `callers`");
  });

  test("uses aft_grep when hoist_builtin_tools is false", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: false,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      disabledTools: new Set(),
    });
    expect(out).toContain("`aft_grep`");
    expect(out).not.toContain("`grep` to locate");
  });

  test("references aft_search only when semantic is enabled", () => {
    const off = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      disabledTools: new Set(),
    });
    expect(off).not.toContain("aft_search");

    const on = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: false,
      disabledTools: new Set(),
    });
    expect(on).toContain("aft_search");
  });

  test("returns null at minimal surface — only safety tool present", () => {
    // At minimal surface, aft_outline + aft_zoom may still be present, but
    // grep is not. Code-exploration section needs both. URL section still
    // works on outline+zoom alone, so we get a non-null block. Test the
    // truly empty case:
    const empty = buildWorkflowHints({
      toolSurface: "minimal",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      // Disable all tools that could produce a hint section. At minimal
      // surface, aft_navigate/grep/aft_search are already absent; disabling
      // outline+zoom kills URL+exploration sections, bash kills the timeout
      // hint, leaving nothing to render.
      disabledTools: new Set(["aft_outline", "aft_zoom", "bash"]),
    });
    expect(empty).toBeNull();
  });

  test("section guarded by disabledTools", () => {
    const out = buildWorkflowHints({
      toolSurface: "all",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: true,
      disabledTools: new Set(["aft_navigate", "bash_status"]),
    });
    // navigate section gated off (aft_navigate disabled).
    expect(out).not.toContain("Use `aft_navigate`");
    // bg-bash section gated off (bash_status disabled) — and there's no
    // 30s fallback anymore, foreground bash auto-promotes silently.
    expect(out).not.toContain("**Long-running commands**");
    expect(out).not.toContain("**Long-running bash commands**");
    expect(out).not.toContain("30 seconds");
    // Other sections survive.
    expect(out).toContain("**Web/URL access**");
    expect(out).toContain("**Code exploration**");
  });
});

describe("buildHintsFromConfig", () => {
  test("emits hints by default", () => {
    const config: AftConfig = { tool_surface: "recommended" };
    const out = buildHintsFromConfig(config, new Set());
    expect(out).not.toBeNull();
    expect(out).toContain("## Prefer AFT tools for token efficiency");
  });

  test("honors hoist_builtin_tools=false (uses aft_grep)", () => {
    const config: AftConfig = { tool_surface: "recommended", hoist_builtin_tools: false };
    const out = buildHintsFromConfig(config, new Set());
    expect(out).toContain("`aft_grep`");
  });

  test("appends bg-bash hint by default on recommended (post-v0.27.2 graduation)", () => {
    // Bash + background are on by default for `recommended` after the bash
    // graduation, so the long-running hint surfaces without explicit opt-in.
    const defaults: AftConfig = { tool_surface: "recommended" };
    expect(buildHintsFromConfig(defaults, new Set())).toContain("**Long-running commands**");
  });

  test("omits bg-bash hint when bash: false (hard opt-out)", () => {
    const off: AftConfig = { tool_surface: "recommended", bash: false };
    expect(buildHintsFromConfig(off, new Set())).not.toContain("**Long-running commands**");
  });

  test("omits bg-bash hint when bash: { background: false }", () => {
    const off: AftConfig = { tool_surface: "recommended", bash: { background: false } };
    expect(buildHintsFromConfig(off, new Set())).not.toContain("**Long-running commands**");
  });

  test("omits bg-bash hint on tool_surface=minimal (bash off by default)", () => {
    const off: AftConfig = { tool_surface: "minimal" };
    expect(buildHintsFromConfig(off, new Set())).not.toContain("**Long-running commands**");
  });

  test("legacy experimental.bash.background=true still enables bg-bash hint", () => {
    const on: AftConfig = {
      tool_surface: "recommended",
      experimental: { bash: { background: true } },
    };
    expect(buildHintsFromConfig(on, new Set())).toContain("**Long-running commands**");
  });
});
