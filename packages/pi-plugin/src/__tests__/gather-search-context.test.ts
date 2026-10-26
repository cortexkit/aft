import { describe, expect, mock, spyOn, test } from "bun:test";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import * as configModule from "../config.js";
import * as logger from "../logger.js";
import { registerSemanticTool } from "../tools/semantic.js";
import {
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

function message(id: string, role: string, content: unknown) {
  return { type: "message", id, message: { role, content } };
}

function storedBranch(label = "current") {
  return [
    message("old-user", "user", "old user text"),
    message("old-assistant", "assistant", [{ type: "text", text: "old assistant text" }]),
    message("user", "user", [
      { type: "text", text: `${label} user` },
      { type: "image", data: "attachment-secret" },
    ]),
    message("assistant", "assistant", [
      { type: "thinking", thinking: `${label} thinking`, thinkingSignature: "signature-secret" },
      { type: "text", text: `${label} text` },
      { type: "toolCall", id: "call-now", name: "aft_search", arguments: { query: "ignored" } },
      { type: "thinking", thinking: "later thinking" },
      { type: "text", text: "later text" },
    ]),
  ];
}

function piGatherFixture({ enabled = true }: { enabled?: boolean } = {}) {
  let branch: unknown[] = storedBranch();
  const getLeafEntry = mock(() => branch.at(-1));
  const getBranch = mock((_fromId?: string) => branch);
  const { api, tools } = makeMockApi();
  const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
  const ctx = makePluginContext(bridge);
  ctx.config.search = { gather: { enabled } };
  const extCtx = {
    ...makeExtContext(),
    sessionManager: { getSessionId: () => "session", getLeafEntry, getBranch },
  } as unknown as ExtensionContext;
  registerSemanticTool(api, ctx);
  const tool = tools.get("aft_search")!;
  return {
    ctx,
    extCtx,
    calls,
    tool,
    getLeafEntry,
    getBranch,
    setBranch(value: unknown[]) {
      branch = value;
    },
    search(
      callId = "call-now",
      args: Record<string, unknown> = { query: "how is config loaded" },
      signal?: AbortSignal,
    ) {
      return tool.execute(callId, args, signal, undefined, extCtx);
    },
  };
}

function expectQueryOnly(fixture: ReturnType<typeof piGatherFixture>) {
  expect(fixture.calls.at(-1)?.params.arguments).toEqual({ query: "how is config loaded" });
  expect(fixture.calls.at(-1)?.options?.searchContext).toBeUndefined();
}

describe("Pi gather context through registration", () => {
  for (const query of [undefined, "", " \t"]) {
    test(`pattern-only query ${String(query)} reads no context`, async () => {
      const f = piGatherFixture();
      await f.search("call-now", { query, pattern: "load_config" });
      expect(f.calls).toHaveLength(1);
      expect(f.getBranch).not.toHaveBeenCalled();
      expect(f.getLeafEntry).not.toHaveBeenCalled();
      expect(f.calls[0].options?.searchContext).toBeUndefined();
    });
  }
  test("whitespace-only thinking, text and user parts do not become context", async () => {
    const fixture = piGatherFixture();
    fixture.setBranch([
      message("user", "user", [{ type: "text", text: " \t\n" }]),
      message("assistant", "assistant", [
        { type: "thinking", thinking: " \n" },
        { type: "text", text: "\t " },
        { type: "toolCall", id: "call-now", name: "aft_search", arguments: {} },
      ]),
    ]);
    await fixture.search();
    expectQueryOnly(fixture);
    fixture.setBranch([
      message("user", "user", " \n"),
      message("assistant", "assistant", [
        { type: "thinking", thinking: "\t" },
        { type: "text", text: "kept" },
        { type: "toolCall", id: "call-now", name: "aft_search", arguments: {} },
      ]),
    ]);
    await fixture.search();
    expect(fixture.calls.at(-1)?.options?.searchContext).toBe("kept");
  });

  test("gatherEnabled reads only an explicit true from live config", () => {
    expect(typeof configModule.gatherEnabled).toBe("function");
    const fixture = piGatherFixture();
    expect(configModule.gatherEnabled(fixture.ctx.config)).toBe(true);
    fixture.ctx.config.search = { gather: { enabled: false } };
    expect(configModule.gatherEnabled(fixture.ctx.config)).toBe(false);
    fixture.ctx.config.search = { gather: {} };
    expect(configModule.gatherEnabled(fixture.ctx.config)).toBe(false);
    fixture.ctx.config.search = {};
    expect(configModule.gatherEnabled(fixture.ctx.config)).toBe(false);
    fixture.ctx.config.search = undefined;
    expect(configModule.gatherEnabled(fixture.ctx.config)).toBe(false);
    Object.assign(fixture.ctx.config, { search: { gather: { enabled: "true" } } });
    expect(configModule.gatherEnabled(fixture.ctx.config)).toBe(false);
  });

  test("gather off never reads the Pi branch", async () => {
    const fixture = piGatherFixture({ enabled: false });
    await fixture.search();
    expect(fixture.getBranch).not.toHaveBeenCalled();
    expect(fixture.getLeafEntry).not.toHaveBeenCalled();
    expectQueryOnly(fixture);
  });

  test("only an explicit true enables branch reads", async () => {
    const fixture = piGatherFixture();
    fixture.ctx.config.search = {};
    await fixture.search();
    expect(fixture.getBranch).not.toHaveBeenCalled();
    expect(fixture.getLeafEntry).not.toHaveBeenCalled();
    expectQueryOnly(fixture);
  });

  test("collects preceding thinking and text with the newest user on the anchored branch", async () => {
    const fixture = piGatherFixture();
    await fixture.search();
    expect(fixture.getBranch).toHaveBeenCalledWith("assistant");
    expect(fixture.calls[0].options?.searchContext).toBe(
      "current thinking\n\ncurrent text\n\ncurrent user",
    );
    expect(fixture.calls[0].params.arguments).toEqual({ query: "how is config loaded" });
  });

  test("forwards context beside abortSignal without changing agent arguments or schema", async () => {
    const fixture = piGatherFixture();
    const args = { query: "needle", pattern: "^export", topK: 3, offset: 2, includeTests: true };
    const signal = new AbortController().signal;
    await fixture.search("call-now", args, signal);
    expect(fixture.calls[0].options?.searchContext).toBe(
      "current thinking\n\ncurrent text\n\ncurrent user",
    );
    expect(fixture.calls[0].options?.abortSignal).toBe(signal);
    expect(fixture.calls[0].params.arguments).toEqual(args);
    expect(args).toEqual({
      query: "needle",
      pattern: "^export",
      topK: 3,
      offset: 2,
      includeTests: true,
    });
    expect(
      (fixture.tool.parameters as { properties: Record<string, unknown> }).properties.searchContext,
    ).toBeUndefined();
    expect(
      (fixture.tool.parameters as { properties: Record<string, unknown> }).properties
        .search_context,
    ).toBeUndefined();
  });

  test("a tool-result leaf never substitutes the older assistant turn", async () => {
    const fixture = piGatherFixture();
    fixture.setBranch([
      ...storedBranch(),
      message("result", "toolResult", [{ type: "text", text: "result-secret" }]),
    ]);
    await fixture.search();
    expectQueryOnly(fixture);
  });

  test("an absent current turn never substitutes an older assistant", async () => {
    const fixture = piGatherFixture();
    fixture.setBranch(storedBranch().slice(0, -1));
    await fixture.search();
    expectQueryOnly(fixture);
  });

  test("a wrong call id returns query-only", async () => {
    const fixture = piGatherFixture();
    await fixture.search("another-call");
    expectQueryOnly(fixture);
  });

  test("an empty leaf returns query-only", async () => {
    const fixture = piGatherFixture();
    fixture.setBranch([]);
    await fixture.search();
    expectQueryOnly(fixture);
  });

  test("a minimal leaf without a stored entry id returns query-only", async () => {
    const fixture = piGatherFixture();
    const branch = storedBranch();
    Object.assign(branch.at(-1)!, { id: undefined });
    fixture.setBranch(branch);
    await fixture.search();
    expect(fixture.getBranch).not.toHaveBeenCalled();
    expectQueryOnly(fixture);
  });

  test("a minimal Pi-shaped session manager without branch APIs returns query-only", async () => {
    const fixture = piGatherFixture();
    fixture.extCtx.sessionManager = makeExtContext(process.cwd(), "minimal-session").sessionManager;
    await fixture.search();
    expectQueryOnly(fixture);
  });

  test("a missing session manager returns query-only", async () => {
    const fixture = piGatherFixture();
    Object.assign(fixture.extCtx, { sessionManager: undefined });
    await fixture.search();
    expectQueryOnly(fixture);
  });

  test("a missing branch API returns query-only", async () => {
    const fixture = piGatherFixture();
    Object.assign(fixture.extCtx.sessionManager, { getBranch: undefined });
    await fixture.search();
    expectQueryOnly(fixture);
  });

  test("a branch read exception returns query-only", async () => {
    const fixture = piGatherFixture();
    fixture.getBranch.mockImplementation(() => {
      throw new Error("branch unavailable");
    });
    await fixture.search();
    expectQueryOnly(fixture);
  });

  test("a leaf read exception returns query-only", async () => {
    const fixture = piGatherFixture();
    fixture.getLeafEntry.mockImplementation(() => {
      throw new Error("leaf unavailable");
    });
    await fixture.search();
    expectQueryOnly(fixture);
  });

  test("a read exception logs only its kind without error or context text", async () => {
    const diagnostic = spyOn(logger, "warn").mockImplementation(() => {});
    try {
      const fixture = piGatherFixture();
      const failure = new Error("private-error-body");
      failure.name = "private-error-name";
      fixture.getBranch.mockImplementation(() => {
        throw failure;
      });
      await fixture.search();
      expectQueryOnly(fixture);
      expect(diagnostic).toHaveBeenCalledTimes(1);
      expect(diagnostic).toHaveBeenCalledWith("gather context unavailable: Error");
    } finally {
      diagnostic.mockRestore();
    }
  });

  test("a non-Error read exception logs only the value type", async () => {
    const diagnostic = spyOn(logger, "warn").mockImplementation(() => {});
    try {
      const fixture = piGatherFixture();
      fixture.getLeafEntry.mockImplementation(() => {
        throw "private-error-body";
      });
      await fixture.search();
      expectQueryOnly(fixture);
      expect(diagnostic).toHaveBeenCalledTimes(1);
      expect(diagnostic).toHaveBeenCalledWith("gather context unavailable: string");
    } finally {
      diagnostic.mockRestore();
    }
  });

  test("a branch that does not end at the captured leaf returns query-only", async () => {
    const fixture = piGatherFixture();
    fixture.getBranch.mockImplementation(() => storedBranch().slice(0, -1));
    await fixture.search();
    expectQueryOnly(fixture);
  });

  test("caps long reasoning to 6000 Unicode characters while retaining the newest user text", async () => {
    const fixture = piGatherFixture();
    fixture.setBranch([
      message("user", "user", "user prefix"),
      message("assistant", "assistant", [
        { type: "thinking", thinking: `discard${"界".repeat(5999)}🙂` },
        { type: "toolCall", id: "call-now", name: "aft_search", arguments: {} },
      ]),
    ]);
    await fixture.search();
    const context = fixture.calls[0].options?.searchContext;
    expect(context === `${"界".repeat(5986)}🙂\n\nuser prefix`).toBe(true);
    expect(Array.from(String(context))).toHaveLength(6000);
    expect(String(context).endsWith("🙂\n\nuser prefix")).toBe(true);
  });

  test("string user content and an assistant without preceding text remain valid", async () => {
    const fixture = piGatherFixture();
    fixture.setBranch([
      message("user", "user", "latest string user"),
      message("assistant", "assistant", [
        { type: "toolCall", id: "call-now", name: "aft_search", arguments: {} },
      ]),
    ]);
    await fixture.search();
    expect(fixture.calls[0].options?.searchContext).toBe("latest string user");
  });

  test("live disable stops further branch reads", async () => {
    const fixture = piGatherFixture();
    await fixture.search();
    expect(fixture.calls[0].options?.searchContext).toBe(
      "current thinking\n\ncurrent text\n\ncurrent user",
    );
    fixture.ctx.config = { ...fixture.ctx.config, search: { gather: { enabled: false } } };
    fixture.getBranch.mockClear();
    fixture.getLeafEntry.mockClear();
    await fixture.search();
    expect(fixture.getBranch).not.toHaveBeenCalled();
    expect(fixture.getLeafEntry).not.toHaveBeenCalled();
    expectQueryOnly(fixture);
  });

  test("concurrent calls capture their own branch before transport work", async () => {
    const fixture = piGatherFixture();
    fixture.setBranch(storedBranch("first"));
    const first = fixture.search();
    expect(fixture.getBranch).toHaveBeenCalledTimes(1);
    fixture.setBranch(storedBranch("second"));
    const second = fixture.search();
    await Promise.all([first, second]);
    expect(fixture.calls.map((call) => call.options?.searchContext)).toEqual([
      "first thinking\n\nfirst text\n\nfirst user",
      "second thinking\n\nsecond text\n\nsecond user",
    ]);
  });
});
