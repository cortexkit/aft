/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import {
  _resetSessionDirectoryCacheForTest,
  getSessionDirectory,
  getSessionDirectoryCached,
  warmSessionDirectory,
} from "../shared/session-directory.js";

afterEach(() => {
  _resetSessionDirectoryCacheForTest();
});

describe("session-directory", () => {
  test("returns null when client lacks session.get", async () => {
    const result = await getSessionDirectory({}, "ses_foo", "/cwd");
    expect(result).toBeNull();
  });

  test("preserves `this` binding when calling SDK session.get (regression: this._client)", async () => {
    // Mirrors the real OpenCode SDK shape where `Session.get` is a
    // class method that depends on `this._client`. Extracting the
    // function reference and calling it without binding crashes
    // with "undefined is not an object (evaluating 'this._client')".
    class FakeSessionApi {
      private readonly _client = { ok: true };
      async get(input: { path: { id: string } }) {
        // Throws if `this` is not the FakeSessionApi instance.
        if (!this._client?.ok)
          throw new Error("undefined is not an object (evaluating 'this._client')");
        return { data: { id: input.path.id, directory: "/from/session" } };
      }
    }
    const client = { session: new FakeSessionApi() };
    const result = await getSessionDirectory(client, "ses_bind", "/cwd");
    expect(result).toBe("/from/session");
  });

  test("returns the session-stored directory when client.session.get resolves", async () => {
    const client = {
      session: {
        get: async (input: { path: { id: string }; directory?: string }) => {
          expect(input).toEqual({ path: { id: "ses_foo" } });
          expect("directory" in input).toBe(false);
          return { data: { id: input.path.id, directory: "/real/project" } };
        },
      },
    };

    const result = await getSessionDirectory(client, "ses_foo", "/cwd");
    expect(result).toBe("/real/project");
    expect(getSessionDirectoryCached("ses_foo")).toBe("/real/project");
  });

  test("supports both wrapped {data: Session} and bare Session response shapes", async () => {
    const wrappedClient = {
      session: {
        get: async () => ({ data: { directory: "/a" } }),
      },
    };
    const bareClient = {
      session: {
        get: async () => ({ directory: "/b" }),
      },
    };

    expect(await getSessionDirectory(wrappedClient, "ses_a", "/cwd")).toBe("/a");
    expect(await getSessionDirectory(bareClient, "ses_b", "/cwd")).toBe("/b");
  });

  test("caches successful lookups so subsequent calls do not refetch", async () => {
    let calls = 0;
    const client = {
      session: {
        get: async () => {
          calls++;
          return { data: { directory: "/cached" } };
        },
      },
    };

    await getSessionDirectory(client, "ses_cached", "/cwd");
    await getSessionDirectory(client, "ses_cached", "/cwd");
    await getSessionDirectory(client, "ses_cached", "/cwd");
    expect(calls).toBe(1);
  });

  test("caches negative results (no session.get) without retrying", async () => {
    const client = {
      session: {
        get: undefined as unknown,
      },
    };

    await getSessionDirectory(client, "ses_missing", "/cwd");
    // Calling again should not retry — the negative cache entry is set on
    // first lookup and a missing fetcher returns null synchronously.
    expect(getSessionDirectoryCached("ses_missing")).toBeNull();
  });

  test("does not cache transient errors (so a temporary failure can recover)", async () => {
    let calls = 0;
    const client = {
      session: {
        get: async () => {
          calls++;
          if (calls === 1) throw new Error("network blip");
          return { data: { directory: "/recovered" } };
        },
      },
    };

    expect(await getSessionDirectory(client, "ses_flaky", "/cwd")).toBeNull();
    // Cache entry should not be set on error — next call retries.
    expect(getSessionDirectoryCached("ses_flaky")).toBeUndefined();
    expect(await getSessionDirectory(client, "ses_flaky", "/cwd")).toBe("/recovered");
  });

  test("returns null and caches when the session has no directory field", async () => {
    const client = {
      session: {
        get: async () => ({ data: {} }),
      },
    };
    expect(await getSessionDirectory(client, "ses_empty", "/cwd")).toBeNull();
    expect(getSessionDirectoryCached("ses_empty")).toBeNull();
  });

  test("getSessionDirectoryCached returns undefined when session has not been looked up", () => {
    expect(getSessionDirectoryCached("ses_unknown")).toBeUndefined();
  });

  test("getSessionDirectoryCached returns undefined for empty sessionId", () => {
    expect(getSessionDirectoryCached("")).toBeUndefined();
    expect(getSessionDirectoryCached(undefined)).toBeUndefined();
  });

  test("warmSessionDirectory triggers async lookup without blocking", async () => {
    type Resolver = (v: { data: { directory: string } }) => void;
    const resolverHolder: { resolve: Resolver | null } = { resolve: null };
    const lookupStarted = new Promise<void>((resolve) => {
      const client = {
        session: {
          get: () => {
            resolve();
            return new Promise<{ data: { directory: string } }>((r) => {
              resolverHolder.resolve = r;
            });
          },
        },
      };
      warmSessionDirectory(client, "ses_warm", "/cwd");
    });

    // The warmup call returns synchronously; the async lookup begins shortly.
    await lookupStarted;
    expect(getSessionDirectoryCached("ses_warm")).toBeUndefined(); // still pending

    resolverHolder.resolve?.({ data: { directory: "/warmed" } });
    // Give the microtask a chance to settle.
    await new Promise((r) => setTimeout(r, 5));
    expect(getSessionDirectoryCached("ses_warm")).toBe("/warmed");
  });

  test("warmSessionDirectory is a no-op when the cache already has the session", async () => {
    let calls = 0;
    const client = {
      session: {
        get: async () => {
          calls++;
          return { data: { directory: "/once" } };
        },
      },
    };
    await getSessionDirectory(client, "ses_w2", "/cwd");
    expect(calls).toBe(1);
    warmSessionDirectory(client, "ses_w2", "/cwd");
    await new Promise((r) => setTimeout(r, 5));
    expect(calls).toBe(1);
  });

  test("warmSessionDirectory is a no-op for empty sessionId", () => {
    let calls = 0;
    const client = {
      session: {
        get: async () => {
          calls++;
          return { data: { directory: "/x" } };
        },
      },
    };
    warmSessionDirectory(client, undefined, "/cwd");
    warmSessionDirectory(client, "", "/cwd");
    expect(calls).toBe(0);
  });
});
