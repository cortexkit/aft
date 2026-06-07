/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import {
  __resetLiveServerWakeForTests,
  probeServerReachable,
  setLiveServerWakeAvailable,
  useLiveServerWake,
} from "../shared/live-server-client.js";

const originalFetch = globalThis.fetch;
const originalServerPassword = process.env.OPENCODE_SERVER_PASSWORD;
const originalServerUsername = process.env.OPENCODE_SERVER_USERNAME;

afterEach(() => {
  globalThis.fetch = originalFetch;
  if (originalServerPassword === undefined) {
    delete process.env.OPENCODE_SERVER_PASSWORD;
  } else {
    process.env.OPENCODE_SERVER_PASSWORD = originalServerPassword;
  }
  if (originalServerUsername === undefined) {
    delete process.env.OPENCODE_SERVER_USERNAME;
  } else {
    process.env.OPENCODE_SERVER_USERNAME = originalServerUsername;
  }
  __resetLiveServerWakeForTests();
});

describe("probeServerReachable", () => {
  test("accepts successful OpenCode API responses", async () => {
    stubFetch(204);

    await expect(probeServerReachable("http://127.0.0.1:4096/")).resolves.toBe(true);
  });

  test("rejects 401/403 auth-protected listeners without usable env auth", async () => {
    stubFetch(401);
    await expect(probeServerReachable("http://127.0.0.1:4096/")).resolves.toBe(false);

    stubFetch(403);
    await expect(probeServerReachable("http://127.0.0.1:4097/")).resolves.toBe(false);

    stubFetch(404);
    await expect(probeServerReachable("http://127.0.0.1:4098/")).resolves.toBe(false);
  });

  test("sends env-derived Authorization header and accepts 2xx when auth succeeds", async () => {
    process.env.OPENCODE_SERVER_USERNAME = "oracle";
    process.env.OPENCODE_SERVER_PASSWORD = "secret";

    let callCount = 0;
    testFetch(async (_input, init) => {
      callCount += 1;
      expect(init?.headers).toEqual({
        Authorization: `Basic ${Buffer.from("oracle:secret").toString("base64")}`,
      });
      return new Response(null, { status: 204 });
    });

    await expect(probeServerReachable("http://127.0.0.1:4096/")).resolves.toBe(true);
    expect(callCount).toBe(1);
  });

  test("records reachability per serverUrl", async () => {
    setLiveServerWakeAvailable("http://127.0.0.1:4096/", true);
    setLiveServerWakeAvailable("http://127.0.0.1:4097/", false);

    expect(useLiveServerWake("http://127.0.0.1:4096/")).toBe(true);
    expect(useLiveServerWake("http://127.0.0.1:4097/")).toBe(false);
    expect(useLiveServerWake("http://127.0.0.1:4098/")).toBe(false);
  });

  test("probe results do not cross-contaminate other serverUrls", async () => {
    stubFetch(204);
    await expect(probeServerReachable("http://127.0.0.1:4096/")).resolves.toBe(true);

    expect(useLiveServerWake("http://127.0.0.1:4096/")).toBe(true);
    expect(useLiveServerWake("http://127.0.0.1:4097/")).toBe(false);
  });

  test("disabled config forces in-process fallback decision", () => {
    setLiveServerWakeAvailable("http://127.0.0.1:4096/", true);

    expect(useLiveServerWake("http://127.0.0.1:4096/", false)).toBe(false);
  });
});

function stubFetch(status: number): void {
  globalThis.fetch = (async () => new Response(null, { status })) as typeof fetch;
}

function testFetch(fn: typeof fetch): typeof fetch {
  globalThis.fetch = fn;
  return fn;
}
