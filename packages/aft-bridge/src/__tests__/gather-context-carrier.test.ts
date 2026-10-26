import { expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type RouteHandle, SocketClosedError } from "@cortexkit/subc-client";
import { BinaryBridge } from "../bridge.js";
import * as bridgeExports from "../index.js";
import { type SubcClientLike, SubcTransportPool } from "../subc-transport.js";
import type { ToolCallOptions } from "../transport.js";
import { TEST_PROJECT_ROOT } from "./subc-test-roots.js";
import { cachedExecutable } from "./test-utils/cached-executable.js";

test("gather cap keeps newest Unicode characters", () => {
  expect(bridgeExports.SEARCH_CONTEXT_MAX_CHARACTERS).toBe(6000);
  const input = `discard${"界".repeat(5999)}🙂`;
  const output = bridgeExports.capSearchContext(input);
  expect(Array.from(output)).toHaveLength(6000);
  expect(output).toBe(`${"界".repeat(5999)}🙂`);
  expect(bridgeExports.capSearchContext("short🙂")).toBe("short🙂");
  expect(bridgeExports.capSearchContext("")).toBe("");
});

test("gather BinaryBridge carries capped search context outside arguments", async () => {
  const executable = cachedExecutable(`#!/usr/bin/env node
const readline = require("node:readline");
readline.createInterface({ input: process.stdin }).on("line", line => {
  const req = JSON.parse(line);
  process.stdout.write(JSON.stringify({ id: req.id, success: true, text: "ok", captured: req }) + "\\n");
});
`);
  const root = mkdtempSync(join(tmpdir(), "gather-carrier-"));
  const bridge = new BinaryBridge(executable, root, { timeoutMs: 10000, maxRestarts: 0 });
  try {
    const args = { query: "how is config loaded" };
    const reply = await bridge.toolCall("s", "search", args, { searchContext: "trace-marker" });
    expect(reply.captured).toMatchObject({ search_context: "trace-marker", arguments: args });
    expect((reply.captured as Record<string, unknown>).arguments).toEqual(args);
    const capped = await bridge.toolCall("s", "aft_search", args, {
      searchContext: `discard${"界".repeat(5999)}🙂`,
    });
    expect((capped.captured as Record<string, unknown>).search_context).toBe(
      `${"界".repeat(5999)}🙂`,
    );
    for (const [name, options] of [
      ["read", { searchContext: "trace-marker" }],
      ["search", {}],
    ] as const) {
      const plain = await bridge.toolCall("s", name, args, options);
      expect(plain.captured).not.toHaveProperty("search_context");
    }
    for (const value of [null, undefined, 42, { marker: "junk" }]) {
      const options = { searchContext: value } as unknown as ToolCallOptions;
      const plain = await bridge.toolCall("s", "search", args, options);
      expect(plain.captured).not.toHaveProperty("search_context");
      expect((plain.captured as Record<string, unknown>).arguments).toEqual(args);
    }
  } finally {
    await bridge.shutdown();
    rmSync(root, { recursive: true, force: true });
  }
});

test.each([
  ["null", null, undefined],
  ["missing", undefined, undefined],
  ["number", 42, undefined],
  ["object", { marker: "junk" }, undefined],
  ["empty", "", ""],
  ["short", "trace-marker", "trace-marker"],
  ["over cap", `discard${"界".repeat(5999)}🙂`, `${"界".repeat(5999)}🙂`],
])("gather BinaryBridge native send handles %s context", async (_label, value, expected) => {
  const executable = cachedExecutable(`#!/usr/bin/env node
const readline = require("node:readline");
readline.createInterface({ input: process.stdin }).on("line", line => {
  const req = JSON.parse(line);
  process.stdout.write(JSON.stringify({ id: req.id, success: true, text: "ok", captured: req }) + "\\n");
});
`);
  const root = mkdtempSync(join(tmpdir(), "gather-native-carrier-"));
  const bridge = new BinaryBridge(executable, root, { timeoutMs: 10000, maxRestarts: 0 });
  try {
    for (const args of [
      { query: "config loading" },
      { query: "config loading", method: "payload" },
    ]) {
      const params = { ...args, search_context: value };
      const reply = await bridge.send("semantic_search", params);
      const captured = reply.captured as Record<string, unknown>;
      if (expected === undefined) {
        expect(captured).not.toHaveProperty("search_context");
      } else {
        expect(captured.search_context).toBe(expected);
      }
      if ("method" in args) {
        expect(captured.params).toEqual(args);
      } else {
        expect(captured).toMatchObject(args);
        expect(captured).not.toHaveProperty("params");
      }
      expect(params).toEqual({ ...args, search_context: value });
    }
    const plain = await bridge.send("status", { search_context: value });
    expect(plain.captured).not.toHaveProperty("search_context");
  } finally {
    await bridge.shutdown();
    rmSync(root, { recursive: true, force: true });
  }
});

test("gather subc carries tool and native search context outside arguments", async () => {
  const bodies: Record<string, unknown>[] = [];
  const client: SubcClientLike = {
    routeOpen: async () => ({ channel: 1, epoch: 1 }) as RouteHandle,
    request: async (_route, body) => {
      bodies.push(body as Record<string, unknown>);
      return { structuredContent: { success: true, text: "ok" } };
    },
    subscribe: () => ({ closed: new Promise<void>(() => {}), unsubscribe() {} }),
    closeRouteChannel: async () => {},
    close() {},
  };
  const pool = new SubcTransportPool({
    connectionFile: "/fake/connection.json",
    harness: "opencode",
    connect: async () => client,
  });
  try {
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    const args = { query: "how is config loaded" };
    await bridge.toolCall("s", "search", args, { searchContext: "trace-marker" });
    await bridge.toolCall("s", "aft_search", args, {
      searchContext: `discard${"界".repeat(5999)}🙂`,
    });
    await bridge.send("semantic_search", { ...args, search_context: "trace-marker" });
    await bridge.toolCall("s", "read", args, { searchContext: "trace-marker" });
    await bridge.toolCall("s", "search", args);
    await bridge.send("status", { search_context: "trace-marker" });
    expect(bodies[0]).toMatchObject({ search_context: "trace-marker", arguments: args });
    expect(bodies[0]?.arguments).toEqual(args);
    expect(bodies[1]?.search_context).toBe(`${"界".repeat(5999)}🙂`);
    expect(bodies[2]).toMatchObject({ search_context: "trace-marker", arguments: args });
    expect(bodies[2]?.arguments).toEqual(args);
    for (const body of bodies.slice(3, 6)) {
      expect(body).not.toHaveProperty("search_context");
      expect(body.arguments).not.toHaveProperty("search_context");
    }
    for (const value of [null, undefined, 42, { marker: "junk" }]) {
      const options = { searchContext: value } as unknown as ToolCallOptions;
      await bridge.send("semantic_search", { ...args, search_context: value });
      expect(bodies[bodies.length - 1]).not.toHaveProperty("search_context");
      await bridge.toolCall("s", "search", args, options);
      for (const body of bodies.slice(-2)) {
        expect(body).not.toHaveProperty("search_context");
        expect(body.arguments).toEqual(args);
      }
    }
  } finally {
    await pool.shutdown();
  }
});

test("gather context remains per call through deferred admission and reconnect", async () => {
  let admit!: () => void;
  let entered!: () => void;
  const admission = new Promise<void>((resolve) => {
    admit = resolve;
  });
  const opening = new Promise<void>((resolve) => {
    entered = resolve;
  });
  const bodies: Array<{ client: number; body: Record<string, unknown> }> = [];
  let clients = 0;
  let fail = false;
  const pool = new SubcTransportPool({
    connectionFile: "/fake/connection.json",
    harness: "opencode",
    connect: async () => {
      const client = ++clients;
      return {
        routeOpen: async () => {
          entered();
          await admission;
          return { channel: client, epoch: 1 } as RouteHandle;
        },
        request: async (_route, body) => {
          if (fail) {
            fail = false;
            throw new SocketClosedError("fixture socket closed");
          }
          bodies.push({ client, body: body as Record<string, unknown> });
          return { structuredContent: { success: true, text: "ok" } };
        },
        subscribe: () => ({ closed: new Promise<void>(() => {}), unsubscribe() {} }),
        closeRouteChannel: async () => {},
        close() {},
      } satisfies SubcClientLike;
    },
  });
  try {
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    const pending = bridge.toolCall(
      "a",
      "search",
      { query: "how is config loaded" },
      { searchContext: "trace-a" },
    );
    await opening;
    expect(bodies).toHaveLength(0);
    admit();
    await pending;
    expect(bodies[0]).toMatchObject({ client: 1, body: { search_context: "trace-a" } });
    fail = true;
    await expect(
      bridge.toolCall(
        "a",
        "search",
        { query: "how is config loaded" },
        { searchContext: "failed-trace" },
      ),
    ).rejects.toThrow();
    await bridge.toolCall(
      "b",
      "search",
      { query: "how is config loaded" },
      { searchContext: "trace-b" },
    );
    expect(clients).toBe(2);
    expect(bodies[1]).toMatchObject({ client: 2, body: { search_context: "trace-b" } });
    await bridge.toolCall("a", "search", { query: "how is config loaded" });
    expect(bodies[2]?.body).not.toHaveProperty("search_context");
    expect(
      bodies.every(
        ({ body }) => !("search_context" in (body.arguments as Record<string, unknown>)),
      ),
    ).toBe(true);
  } finally {
    admit();
    await pool.shutdown();
  }
});
