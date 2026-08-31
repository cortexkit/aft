/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { __test__ } from "../index.js";
import {
  parseInspectTerminal,
  registerInspectTool,
  renderInspectTerminal,
} from "../tools/inspect.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

function resultText(result: unknown): string {
  const typed = result as { content?: Array<{ text?: string }> };
  return typed.content?.[0]?.text ?? "";
}

function freshTerminal() {
  return {
    success: true,
    terminal: "FRESH",
    text: "fresh result body",
    wait_stamp: {
      text: "waited: yes; completed: lsp_start,tier2_rescan",
      phases: [
        { id: "lsp_start", producer: "tsserver" },
        { id: "tier2_rescan", category: "dead_code", also_satisfied: ["unused_exports"] },
      ],
    },
  };
}

describe("Pi aft_inspect surface", () => {
  test("registers at recommended surface unless explicitly disabled", () => {
    expect(__test__.resolveToolSurface({ tool_surface: "recommended" }).inspect).toBe(true);
    expect(__test__.resolveToolSurface({ tool_surface: "minimal" }).inspect).toBe(false);
    expect(
      __test__.resolveToolSurface({
        tool_surface: "recommended",
        disabled_tools: ["aft_inspect"],
      }).inspect,
    ).toBe(false);
  });

  test("documents blocking-fresh results, scope narrowing, and the alert channel", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => freshTerminal());
    registerInspectTool(api, makePluginContext(bridge));

    const inspect = tools.get("aft_inspect")!;
    const description = inspect.description ?? "";
    expect(description).toContain("Blocking-fresh");
    expect(description).toContain("wait-stamp");
    expect(description).toContain("alert channel");
    expect(description).not.toContain("short deadline");
    expect(description).not.toContain("pending_categories");
    expect(description).not.toContain("background warmup");

    const parameters = inspect.parameters as {
      properties?: Record<string, Record<string, unknown>>;
    };
    expect(parameters.properties?.scope?.description).toContain("`scope=` narrows results");
    expect(parameters.properties?.scope?.description).not.toContain("Tier 1 scopes the scan");
  });

  test("parses the shared phase-entry shape for every terminal form", () => {
    const fresh = parseInspectTerminal(freshTerminal());
    const interrupted = parseInspectTerminal({
      terminal: "INTERRUPTED",
      phases: [{ id: "lsp_quiescence", producer: "rust_analyzer" }],
    });
    const failed = parseInspectTerminal({
      success: false,
      terminal: "PHASE-FAILED",
      phases: [{ id: "lsp_start", producer: "tsserver" }],
      failed_phase: "lsp_start",
      producer: "tsserver",
      failure_reason: "server_start_failed",
      failure_detail: "binary exited",
    });
    const preflight = parseInspectTerminal({
      success: false,
      terminal: "PHASE-FAILED",
      phases: [],
      failure_reason: "missing_executable",
    });

    expect(fresh).toMatchObject({
      kind: "FRESH",
      waitStampText: "waited: yes; completed: lsp_start,tier2_rescan",
    });
    expect(fresh?.phases[0]).toEqual({
      id: "lsp_start",
      producer: "tsserver",
      category: undefined,
      alsoSatisfied: [],
    });
    expect(fresh?.phases[1]?.alsoSatisfied).toEqual(["unused_exports"]);
    expect(interrupted?.phases[0]).toMatchObject({
      id: "lsp_quiescence",
      producer: "rust_analyzer",
    });
    expect(failed).toMatchObject({
      kind: "PHASE-FAILED",
      failedPhase: { id: "lsp_start", producer: "tsserver" },
      failureReason: "server_start_failed",
      failureDetail: "binary exited",
    });
    expect(preflight).toMatchObject({
      kind: "PHASE-FAILED",
      phases: [],
      failedPhase: undefined,
      failureReason: "missing_executable",
    });
    expect(renderInspectTerminal(preflight!)).toContain("(missing_executable)");
  });

  test("renders phase failures with detail and bounded retry guidance", () => {
    const terminal = parseInspectTerminal({
      terminal: "PHASE-FAILED",
      completed_phases: [{ id: "stat_verification" }],
      failure_reason: "inspect_not_fresh",
      failure_detail: "metrics did not complete",
    });

    const rendered = renderInspectTerminal(terminal!, "request failed");
    expect(rendered).toContain(
      "inspect could not complete: metrics did not complete (inspect_not_fresh).",
    );
    expect(rendered).toContain("Completed phases: 1. Retry, or narrow with sections=...");
    expect(rendered).not.toContain("request failed");
  });

  test("renders interrupted terminals with safe retry guidance", () => {
    const terminal = parseInspectTerminal({
      terminal: "INTERRUPTED",
      completed_phases: [{ id: "lsp_quiescence" }],
    });

    const rendered = renderInspectTerminal(terminal!, "request failed");
    expect(rendered).toContain("inspect was interrupted");
    expect(rendered).toContain("Retry is safe");
    expect(rendered).not.toContain("request failed");
  });

  test("delivers one rendered terminal without a follow-up bridge call", async () => {
    for (const response of [
      freshTerminal(),
      {
        success: false,
        terminal: "INTERRUPTED",
        text: "interrupted result body",
        phases: [{ id: "stat_verification", category: "duplicates" }],
      },
      {
        success: false,
        terminal: "PHASE-FAILED",
        text: "failed result body",
        phases: [{ id: "tier2_rescan", category: "dead_code" }],
        failed_phase: "tier2_rescan",
        category: "dead_code",
        failure_reason: "tier2_rescan_errored",
      },
      {
        success: false,
        terminal: "PHASE-FAILED",
        text: "preflight failure body",
        phases: [],
        failure_reason: "missing_executable",
      },
    ]) {
      const { api, tools } = makeMockApi();
      const { bridge, calls } = makeMockBridge(() => response);
      registerInspectTool(api, makePluginContext(bridge));

      const result = await executeTool(
        tools.get("aft_inspect")!,
        {},
        makeExtContext(projectRoot, "pi-session"),
      );

      expect(calls).toHaveLength(1);
      expect(calls[0]?.command).toBe("tool_call");
      expect(calls[0]?.options).not.toHaveProperty("keepBridgeOnTimeout");
      if (response.terminal === "INTERRUPTED") {
        expect(resultText(result)).toContain("inspect was interrupted");
      } else if (response.terminal === "PHASE-FAILED") {
        expect(resultText(result)).toContain("inspect could not complete");
      } else {
        expect(resultText(result)).toContain(response.terminal);
      }
    }
  });

  test("does not retry after a transport error", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => {
      throw new Error("transport unavailable");
    });
    registerInspectTool(api, makePluginContext(bridge));

    await expect(
      executeTool(tools.get("aft_inspect")!, {}, makeExtContext(projectRoot, "pi-session")),
    ).rejects.toThrow("transport unavailable");
    expect(calls).toHaveLength(1);
    expect(calls[0]?.command).toBe("tool_call");
  });

  test("caps the default diagnostics and transport deadlines below Pi's hard limit", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => freshTerminal());
    registerInspectTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_inspect")!, {}, makeExtContext(projectRoot, "pi-session"));

    expect(calls[0]?.params.arguments).toMatchObject({ diagnostics_timeout_ms: 24_000 });
    expect(calls[0]?.options).toMatchObject({ transportTimeoutMs: 25_000 });
  });

  test("caps configured inspect diagnostics budgets below Pi's hard limit", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => freshTerminal());
    registerInspectTool(
      api,
      makePluginContext(bridge, { config: { inspect: { diagnostics_timeout_ms: 180_000 } } }),
    );

    await executeTool(
      tools.get("aft_inspect")!,
      { sections: "todos", scope: ["src", "tests"], topK: 9 },
      makeExtContext(projectRoot, "pi-session"),
    );

    expect(calls[0]?.params.arguments).toEqual({
      sections: "todos",
      scope: ["src", "tests"],
      topK: 9,
      diagnostics_timeout_ms: 24_000,
    });
    expect(calls[0]?.options).toMatchObject({ transportTimeoutMs: 25_000 });
    expect(calls[0]?.options).not.toHaveProperty("keepBridgeOnTimeout");
  });

  test("rejects invalid topK without a bridge call", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => freshTerminal());
    registerInspectTool(api, makePluginContext(bridge));

    await expect(
      executeTool(tools.get("aft_inspect")!, { topK: "9" }, makeExtContext(projectRoot)),
    ).rejects.toThrow("topK must be an integer between 1 and 100");
    expect(calls).toHaveLength(0);
  });
});

describe("blocking-fresh release manifest", () => {
  test("requires both recorded sweep boundaries on the plugin release entry", () => {
    const manifestPath = fileURLToPath(
      new URL("../../../../docs/v0.49-release-manifest.json", import.meta.url),
    );
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as {
      campaign_entries?: Array<Record<string, unknown>>;
    };
    const entry = manifest.campaign_entries?.find(
      (candidate) => candidate.entry_key === "blocking-fresh-inspect-plugin-release",
    );

    expect(entry).toBeDefined();
    expect(entry).toMatchObject({
      pi_follow_up_retirement: { removed: true },
      terminal_shape_consumption: { pi: true, opencode: true },
      prefix_cache_bust: { required: true },
      health_digest_agent_tool_description_change: false,
    });
    for (const key of ["opencode_inspect_follow_up_sweep", "no_new_knob_sweep"] as const) {
      expect(entry?.[key]).toMatchObject({
        searched_surface: expect.anything(),
        revision: expect.any(String),
      });
    }
  });
});
