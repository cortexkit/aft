import { expect, test } from "bun:test";
import type { AftProjectTransport } from "../transport.js";
import { mintViewToken } from "../view-token.js";

test("view token uses a management request, not an agent tool", async () => {
  const calls: unknown[] = [];
  const transport = {
    send: async (...args: unknown[]) => {
      calls.push(args);
      return { success: true, token: "opaque", expires_ms: 100, advisory: true };
    },
  } as unknown as AftProjectTransport;
  expect(await mintViewToken(transport, "/future/root", "abc")).toEqual({
    token: "opaque",
    expires_ms: 100,
    advisory: true,
  });
  expect(calls).toEqual([["view_token", { root: "/future/root", expected_tree: "abc" }]]);
});

test("view token refuses a non-advisory or failed response", async () => {
  for (const response of [
    { success: false, code: "VIEW_TOKEN", message: "views disabled" },
    { success: true, token: "opaque", expires_ms: 100, advisory: false },
  ]) {
    const transport = { send: async () => response } as unknown as AftProjectTransport;
    await expect(mintViewToken(transport, "/future/root", "abc")).rejects.toThrow();
  }
});
