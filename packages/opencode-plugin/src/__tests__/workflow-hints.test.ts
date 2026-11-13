/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import type { AftConfig } from "../config.js";
import { buildHintsFromConfig, buildWorkflowHints } from "../workflow-hints.js";

describe("buildWorkflowHints", () => {
  test("renders the trimmed guidance with every tool registered", () => {
    const out = buildWorkflowHints({
      bashBackgroundEnabled: true,
      bashCompressionEnabled: true,
      disabledTools: new Set(),
    });
    expect(out).toContain("# Agent Foundational Toolkit");
    expect(out).toContain("**Finding code**: start with aft_search;");
    expect(out).toContain("**Relationships**: aft_callgraph for callers, impact,");
    expect(out).toContain("**After edits**: run aft_inspect for diagnostics;");
    expect(out).toContain("**Commands**: don't pipe test or build output");
    expect(out).not.toContain("Reflex translations:");
    expect(out).not.toContain("**PTY / interactive commands**");
  });

  test("omits the navigation sentence rather than naming disabled zoom", () => {
    const out = buildWorkflowHints({
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(["aft_zoom"]),
    });
    expect(out).not.toContain("aft_zoom");
    expect(out).not.toContain("Then aft_outline");
    expect(out).not.toContain("**Web pages**");
    expect(out).toContain("start with aft_search;");
  });

  test("returns null when only the safety tool is registered", () => {
    expect(
      buildWorkflowHints({
        bashBackgroundEnabled: false,
        bashCompressionEnabled: false,
        disabledTools: new Set([
          "aft_outline",
          "aft_zoom",
          "aft_search",
          "aft_callgraph",
          "aft_inspect",
          "grep",
          "read",
          "bash",
          "bash_status",
        ]),
      }),
    ).toBeNull();
  });
});

describe("buildHintsFromConfig", () => {
  test("emits hints by default", () => {
    const config: AftConfig = {};
    const out = buildHintsFromConfig(config, new Set());
    expect(out).not.toBeNull();
    expect(out).toContain("# Agent Foundational Toolkit");
  });

  test("appends bg-bash hint by default (post-v0.27.2 graduation)", () => {
    // Bash + background are on by default for `recommended` after the bash
    // graduation, so the long-running hint surfaces without explicit opt-in.
    const defaults: AftConfig = {};
    expect(buildHintsFromConfig(defaults, new Set())).toContain("wait: true");
  });

  test("omits bg-bash hint when bash: false (hard opt-out)", () => {
    const off: AftConfig = { bash: false };
    expect(buildHintsFromConfig(off, new Set())).not.toContain("wait: true");
  });

  test("omits bg-bash hint when bash: { background: false }", () => {
    const off: AftConfig = { bash: { background: false } };
    expect(buildHintsFromConfig(off, new Set())).not.toContain("wait: true");
  });

  test("omits bg-bash hint when bash and its companions are not registered", () => {
    const config: AftConfig = { disabled_tools: ["bash", "bash_status"] };
    expect(buildHintsFromConfig(config, new Set(["bash", "bash_status"]))).not.toContain(
      "wait: true",
    );
  });

  test("bash.background: true enables the bg-bash hint", () => {
    const on: AftConfig = {
      bash: { background: true },
    };
    expect(buildHintsFromConfig(on, new Set())).toContain("wait: true");
  });
});

describe("hints name only registered companions and enabled bash features", () => {
  const REWRITE_CLAUSE = "(and accepted AFT `cat`/`head`/`tail` rewrites)";

  test("the hashline hint names bash rewrites only while bash is registered and rewrite is on", () => {
    expect(buildHintsFromConfig({}, new Set(), true)).toContain(REWRITE_CLAUSE);
    expect(buildHintsFromConfig({ bash: { rewrite: false } }, new Set(), true)).not.toContain(
      REWRITE_CLAUSE,
    );
    expect(buildHintsFromConfig({}, new Set(["bash"]), true)).not.toContain(REWRITE_CLAUSE);
    expect(buildHintsFromConfig({ bash: { rewrite: false } }, new Set(), true)).toContain(
      "**Hashline edit tags**: Only `read` mint hashline tags.",
    );
  });

  test("background guidance needs watch, but no longer needs the PTY writer", () => {
    const noWatch = buildHintsFromConfig({}, new Set(["bash_watch"])) ?? "";
    expect(noWatch).not.toContain("wait: true");
    expect(noWatch).toContain("**Commands**: don't pipe test or build output");
    const noWrite = buildHintsFromConfig({}, new Set(["bash_write"])) ?? "";
    expect(noWrite).toContain("wait: true");
    expect(noWrite).not.toContain("bash_write");
  });
});
