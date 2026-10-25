import { expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { RouteHandle } from "@cortexkit/subc-client";
import { BinaryBridge } from "../bridge.js";
import * as bridgeExports from "../index.js";
import { type SubcClientLike, SubcTransportPool } from "../subc-transport.js";
import { TEST_PROJECT_ROOT } from "./subc-test-roots.js";
import { cachedExecutable } from "./test-utils/cached-executable.js";

test("gather cap keeps newest Unicode characters", () => {
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
    await bridge.send("semantic_search", { ...args, search_context: 42 });
    expect(bodies[0]).toMatchObject({ search_context: "trace-marker", arguments: args });
    expect(bodies[0]?.arguments).toEqual(args);
    expect(bodies[1]?.search_context).toBe(`${"界".repeat(5999)}🙂`);
    expect(bodies[2]).toMatchObject({ search_context: "trace-marker", arguments: args });
    expect(bodies[2]?.arguments).toEqual(args);
    for (const body of bodies.slice(3, 6)) {
      expect(body).not.toHaveProperty("search_context");
      expect(body.arguments).not.toHaveProperty("search_context");
    }
    expect(bodies[6]?.search_context).toBe(42);
    expect(bodies[6]?.arguments).toEqual(args);
  } finally {
    await pool.shutdown();
  }
});
