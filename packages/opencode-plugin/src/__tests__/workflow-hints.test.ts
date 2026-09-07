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
      bashCompressionEnabled: true,
      disabledTools: new Set(),
    });
    expect(out).not.toBeNull();
    expect(out).toContain("## IMPORTANT NOTICE about your tools");
    // Opening notice: the agent is told its tool set is non-standard and to
    // reach for it first, before any individual section.
    expect(out).toContain("You are equipped with a non-standard tool set");
    expect(out).toContain("Always reach for these tools first");
    expect(out).toContain("**Parallel tool calls**");
    expect(out).toContain("emit them in ONE response instead of serializing");
    expect(out).toContain("**Codebase health & diagnostics**");
    expect(out).toContain("**Web/URL access**");
    expect(out).toContain("**Code exploration**");
    expect(out).toContain("`aft_search` is the primary code-search tool");
    expect(out).not.toContain("hint");
    expect(out).toContain("auto-routes concepts, identifiers, regex");
    // Imperative anti-bash-grep steer with concrete reflex translations.
    expect(out).toContain("DO NOT run `grep`/`rg`/`find`/`sed`/`cat` through `bash`");
    expect(out).toContain("the bash path is unindexed, unranked, serial");
    expect(out).toContain("Reflex translations:");
    expect(out).toContain('aft_search({ query: "handleAuth" })');
    expect(out).toContain("Use `aft_callgraph`");
    expect(out).toContain("- `callers`");
    expect(out).toContain("- `impact`");
    expect(out).toContain("- `trace_to`");
    expect(out).toContain("- `trace_data`");
    expect(out).toContain("**Codebase health & diagnostics**");
    expect(out).toContain("`aft_inspect`");
    expect(out).toContain("diagnostics");
    expect(out).toContain("before you run tests or commit");
    expect(out).toContain("does not surface compile/type errors automatically");
    expect(out).toContain("**Long-running commands**");
    // Foreground-default guidance: foreground is the one-step path, background is
    // only for when there's other work to overlap, and background-then-watch is
    // called out as the anti-pattern it is.
    expect(out).toContain("run them in the FOREGROUND");
    expect(out).toContain("wait: true");
    expect(out).toContain("auto-promote can hand you a reminder");
    expect(out).toContain("`background: true` is ONLY for when you have OTHER useful work");
    expect(out).toContain("Do NOT background a command and then immediately `bash_watch` it");
    expect(out).toContain("the user can interrupt");
    expect(out).toContain("Never loop `bash_status`");
  });

  test("replaces zoom steering with read when zoom is disabled", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(["aft_zoom"]),
    });
    expect(out).toContain("→ `read` for symbol(s)");
    expect(out).not.toContain("aft_zoom");
  });

  test("omits long-running bash hint when background bash is off (foreground auto-promotes)", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
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

  test("shows pipe guidance only when compression is enabled", () => {
    const on = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: true,
      disabledTools: new Set(),
    });
    expect(on).toContain("bash output is auto-compressed for non-piped commands");
    expect(on).toContain("Piped commands run verbatim and show the pipeline's output");
    expect(on).toContain("`bun test | grep fail` → run `bun test`");
    // The agent can't check the config — the section is gated instead of hedged.
    expect(on).not.toContain("compression is on,");

    const off = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(off).not.toContain("bash output is auto-compressed");
    expect(off).not.toContain("`bun test | grep fail`");
  });

  test("omits the navigate section at tool_surface=recommended", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(out).not.toContain("Use `aft_callgraph`");
    expect(out).not.toContain("- `callers`");
  });

  test("uses aft_grep when hoist_builtin_tools is false", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: false,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
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
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(off).not.toContain("aft_search");

    const on = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(on).toContain("aft_search");
  });

  test("inspect hint is gated by registered tool availability", () => {
    const registered = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(registered).toContain("**Codebase health & diagnostics**");
    expect(registered).toContain("aft_inspect");

    const minimal = buildWorkflowHints({
      toolSurface: "minimal",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(minimal).not.toContain("**Codebase health & diagnostics**");
    expect(minimal).not.toContain("aft_inspect");
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
      bashCompressionEnabled: false,
      // Disable all tools that could produce a hint section. At minimal
      // surface, aft_callgraph/grep/aft_search are already absent; disabling
      // outline+zoom kills URL+exploration sections, bash kills the timeout
      // hint, leaving nothing to render.
      disabledTools: new Set(["aft_outline", "aft_zoom", "bash"]),
    });
    // null proves the parallel-tool-call frame is never emitted on its own
    // (unshift runs only when sections already have content).
    expect(empty).toBeNull();
  });

  test("section guarded by disabledTools", () => {
    const out = buildWorkflowHints({
      toolSurface: "all",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: true,
      bashCompressionEnabled: true,
      disabledTools: new Set(["aft_callgraph", "bash_status"]),
    });
    // navigate section gated off (aft_callgraph disabled).
    expect(out).not.toContain("Use `aft_callgraph`");
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
    expect(out).toContain("## IMPORTANT NOTICE about your tools");
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

  test("legacy background=true still enables bg-bash hint", () => {
    const on: AftConfig = {
      tool_surface: "recommended",
      experimental: { bash: { background: true } },
    };
    expect(buildHintsFromConfig(on, new Set())).toContain("**Long-running commands**");
  });
});
