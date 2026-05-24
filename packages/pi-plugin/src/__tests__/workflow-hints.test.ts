/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import type { AftConfig } from "../config.js";
import { buildHintsFromConfig, buildWorkflowHints } from "../workflow-hints.js";

describe("Pi buildWorkflowHints", () => {
  test("renders all four sections at tool_surface=all with bg + semantic", () => {
    const out = buildWorkflowHints({
      toolSurface: "all",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: true,
      absentTools: new Set(),
    });
    expect(out).not.toBeNull();
    expect(out).toContain("## Prefer AFT tools for token efficiency");
    expect(out).toContain("**Web/URL access**");
    expect(out).toContain('`aft_outline({ target: "<url>" })`');
    expect(out).not.toContain("aft_outline({ url })");
    expect(out).toContain("**Code exploration**");
    expect(out).toContain("For exact identifiers (`useState`, function names, env vars)");
    expect(out).toContain(
      "For broad concepts ('where is X handled', 'how does Y work') → `aft_search`",
    );
    expect(out).toContain("Use `aft_navigate`");
    expect(out).toContain("**Long-running commands**");
    // Anti-polling guidance must be present so agents stop calling
    // bash_status back-to-back. Mirrors OpenCode plugin parity.
    expect(out).toContain("A completion reminder is delivered automatically");
    expect(out).toContain("do not poll");
    expect(out).toContain("`task_id`");
    expect(out).toContain("`bash_status({ task_id })`");
    expect(out).not.toContain("taskId");
  });

  test("omits bg-bash section when background is disabled", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      absentTools: new Set(),
    });
    expect(out).not.toContain("**Long-running commands**");
  });

  test("omits navigate at recommended surface", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      absentTools: new Set(),
    });
    expect(out).not.toContain("Use `aft_navigate`");
  });

  test("returns null when all sections gated off by absentTools", () => {
    const out = buildWorkflowHints({
      toolSurface: "minimal",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      absentTools: new Set(["aft_outline", "aft_zoom"]),
    });
    expect(out).toBeNull();
  });
});

describe("Pi buildHintsFromConfig", () => {
  test("emits hints by default and includes hoisted bash name", () => {
    const config: AftConfig = {
      tool_surface: "recommended",
      experimental: { bash: { background: true } },
    };
    const out = buildHintsFromConfig(config, new Set(), true);
    expect(out).not.toBeNull();
    expect(out).toContain("`bash({ background: true })`");
  });
});
