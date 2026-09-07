/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import type { AftConfig } from "../config.js";
import {
  buildHintsFromConfig,
  buildWorkflowHints,
  HASHLINE_TAG_SOURCE_HINT,
} from "../workflow-hints.js";

describe("Pi hashline tag-source guidance", () => {
  const opts = {
    toolSurface: "all" as const,
    hoistBuiltins: true,
    semanticEnabled: true,
    bashBackgroundEnabled: true,
    bashCompressionEnabled: true,
    absentTools: new Set<string>(),
  };

  test("legacy edit users see byte-identical text", () => {
    const legacy = buildWorkflowHints(opts);
    expect(buildWorkflowHints({ ...opts, hashlineEffective: false })).toBe(legacy);
    expect(legacy).not.toContain("mint hashline tags");
  });

  test("hashline sessions are told which tools mint tags", () => {
    const legacy = buildWorkflowHints(opts);
    expect(buildWorkflowHints({ ...opts, hashlineEffective: true })).toBe(
      `${legacy}\n\n${HASHLINE_TAG_SOURCE_HINT}`,
    );
    expect(HASHLINE_TAG_SOURCE_HINT).toContain(
      "Only `read` (and accepted AFT `cat`/`head`/`tail` rewrites) mint hashline tags. `aft_zoom`, `aft_outline`, `grep`, `aft_search`, and conflict snippets do not. After navigation, call `read` on every file and range the patch addresses.",
    );
  });
});

describe("Pi buildWorkflowHints", () => {
  test("renders all four sections at tool_surface=all with bg + semantic", () => {
    const out = buildWorkflowHints({
      toolSurface: "all",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: true,
      bashCompressionEnabled: true,
      absentTools: new Set(),
    });
    expect(out).not.toBeNull();
    expect(out).toContain("## IMPORTANT NOTICE about your tools");
    // Opening notice: non-standard tool set, reach for it first (parity).
    expect(out).toContain("You are equipped with a non-standard tool set");
    expect(out).toContain("Always reach for these tools first");
    expect(out).toContain("**Parallel tool calls**");
    expect(out).toContain("emit them in ONE response instead of serializing");
    expect(out).toContain("**Web/URL access**");
    expect(out).toContain('`aft_outline({ target: "<url>" })`');
    expect(out).not.toContain("aft_outline({ url })");
    expect(out).toContain("**Code exploration**");
    expect(out).toContain("`aft_search` is the primary code-search tool");
    expect(out).not.toContain("hint");
    expect(out).toContain("auto-routes concepts, identifiers, regex");
    // Imperative anti-bash-grep steer with concrete reflex translations (parity).
    expect(out).toContain("DO NOT run `grep`/`rg`/`find`/`sed`/`cat` through `bash`");
    expect(out).toContain("the bash path is unindexed, unranked, serial");
    // Portable bash code-search STOP steer
    expect(out).toContain(
      "If you are about to run grep, rg, sed, awk, find, or cat through bash to locate or read code: STOP — use `aft_search` (concepts, identifiers, regex, literals), `read`, `aft_outline`, or `aft_zoom` instead.",
    );
    expect(out).toContain("Reflex translations:");
    expect(out).toContain('aft_search({ query: "handleAuth" })');
    expect(out).toContain("Use `aft_callgraph`");
    expect(out).toContain("**Codebase health & diagnostics**");
    expect(out).toContain("`aft_inspect`");
    expect(out).toContain("diagnostics");
    expect(out).toContain("before you run tests or commit");
    expect(out).toContain("does not surface compile/type errors automatically");
    expect(out).toContain("**Long-running commands**");
    // Foreground-default guidance (parity with OpenCode).
    expect(out).toContain("run them in the FOREGROUND");
    expect(out).toContain("wait: true");
    expect(out).toContain("auto-promote can hand you a reminder");
    expect(out).toContain("`background: true` is ONLY for when you have OTHER useful work");
    expect(out).toContain("Do NOT background a command and then immediately `bash_watch` it");
    expect(out).toContain("the user can interrupt");
    expect(out).toContain("Never loop `bash_status`");
    expect(out).not.toContain("taskId");
  });

  test("replaces zoom steering with read when zoom is disabled", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(["aft_zoom"]),
    });
    expect(out).toContain("→ `read` for symbol(s)");
    expect(out).not.toContain("aft_zoom");
    expect(out).toContain(
      "If you are about to run grep, rg, sed, awk, find, or cat through bash to locate or read code: STOP — use `aft_search` (concepts, identifiers, regex, literals), `read`, `aft_outline` instead.",
    );
  });

  test("omits bash STOP steer when bash is absent", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(["bash"]),
    });
    expect(out).not.toContain("STOP —");
  });

  test("steers to grep tool when aft_search is absent", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(),
    });
    expect(out).toContain(
      "If you are about to run grep, rg, sed, awk, find, or cat through bash to locate or read code: STOP — use the `grep` tool, `read`, `aft_outline`, or `aft_zoom` instead.",
    );
  });

  test("omits bg-bash section when background is disabled", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(),
    });
    expect(out).not.toContain("**Long-running commands**");
  });

  test("shows pipe guidance only when compression is enabled", () => {
    const on = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: true,
      absentTools: new Set(),
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
      absentTools: new Set(),
    });
    expect(off).not.toContain("bash output is auto-compressed");
    expect(off).not.toContain("`bun test | grep fail`");
  });

  test("omits navigate at recommended surface", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(),
    });
    expect(out).not.toContain("Use `aft_callgraph`");
  });

  test("inspect hint is gated by registered tool availability", () => {
    const registered = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(),
    });
    expect(registered).toContain("**Codebase health & diagnostics**");
    expect(registered).toContain("aft_inspect");

    const minimal = buildWorkflowHints({
      toolSurface: "minimal",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(),
    });
    expect(minimal).not.toContain("**Codebase health & diagnostics**");
    expect(minimal).not.toContain("aft_inspect");
  });

  test("returns null when all sections gated off by absentTools", () => {
    const out = buildWorkflowHints({
      toolSurface: "minimal",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(["aft_outline", "aft_zoom"]),
    });
    // null proves the parallel-tool-call frame is never emitted on its own
    // (unshift runs only when sections already have content).
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
    // Hoisted bash name (bash, not aft_bash) appears in the foreground-default
    // long-running guidance.
    expect(out).toContain("`bash({ command, wait: true })`");
    expect(out).toContain("**Codebase health & diagnostics**");
  });

  test("uses prefixed file and bash names when hoisting is disabled", () => {
    const config: AftConfig = {
      tool_surface: "recommended",
      hoist_builtin_tools: false,
      bash: true,
    };
    const out = buildHintsFromConfig(config, new Set(), false);
    expect(out).toContain("`aft_read({ path:");
    expect(out).toContain("`aft_bash({ command, wait: true })`");
  });
});
