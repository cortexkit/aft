import { describe, expect, mock, test } from "bun:test";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
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
      "current user\ncurrent thinking\ncurrent text",
    );
    expect(fixture.calls[0].params.arguments).toEqual({ query: "how is config loaded" });
  });

  test("forwards context beside abortSignal without changing agent arguments or schema", async () => {
    const fixture = piGatherFixture();
    const args = { query: "needle", pattern: "^export", topK: 3, offset: 2, includeTests: true };
    const signal = new AbortController().signal;
    await fixture.search("call-now", args, signal);
    expect(fixture.calls[0].options?.searchContext).toBe(
      "current user\ncurrent thinking\ncurrent text",
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

  test("OMP minimal contexts without branch APIs return query-only", async () => {
    const fixture = piGatherFixture();
    fixture.extCtx.sessionManager = makeExtContext(process.cwd(), "omp-session").sessionManager;
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

  test("a branch that does not end at the captured leaf returns query-only", async () => {
    const fixture = piGatherFixture();
    fixture.getBranch.mockImplementation(() => storedBranch().slice(0, -1));
    await fixture.search();
    expectQueryOnly(fixture);
  });

  test("caps the joined trace to the newest 6000 Unicode characters", async () => {
    const fixture = piGatherFixture();
    fixture.setBranch([
      message("user", "user", "user prefix"),
      message("assistant", "assistant", [
        { type: "thinking", thinking: `discard${"界".repeat(5999)}🙂` },
        { type: "toolCall", id: "call-now", name: "aft_search", arguments: {} },
      ]),
    ]);
    await fixture.search();
    expect(fixture.calls[0].options?.searchContext).toBe(`${"界".repeat(5999)}🙂`);
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
      "current user\ncurrent thinking\ncurrent text",
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
      "first user\nfirst thinking\nfirst text",
      "second user\nsecond thinking\nsecond text",
    ]);
  });
});
