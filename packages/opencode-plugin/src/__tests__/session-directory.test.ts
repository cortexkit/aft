/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, mock, test } from "bun:test";
import { Effect } from "effect";
import {
  _resetSessionDirectoryCacheForTest,
  getSessionDirectory,
  getSessionDirectoryCached,
  getSessionDirectoryForDeletedEvent,
  getSessionDirectoryForEvent,
  verifySessionDirectory,
  warmSessionDirectory,
} from "../shared/session-directory.js";

afterEach(() => {
  _resetSessionDirectoryCacheForTest();
});

describe("session event directories", () => {
  test("only the owning V2 Location accepts a resolved session, including a shared cached lookup", async () => {
    const session = { location: { directory: "/project/first" } };
    const first = {
      location: { directory: "/project/first" },
      session: { get: () => Effect.succeed(session) },
    };
    const second = {
      location: { directory: "/project/second" },
      session: { get: () => Effect.succeed(session) },
    };
    expect(await getSessionDirectoryForEvent(first, "ses_owned", "/project/first")).toBe(
      "/project/first",
    );
    expect(await getSessionDirectoryForEvent(second, "ses_owned", "/project/second")).toBeNull();
  });

  test("a missing session is not assigned to either V2 Location", async () => {
    for (const directory of ["/project/first", "/project/second"]) {
      const client = {
        location: { directory },
        session: { get: () => Effect.succeed(undefined) },
      };
      expect(await getSessionDirectoryForEvent(client, "ses_missing", directory)).toBeNull();
    }
  });

  test("V2 workspaces sharing a directory still have distinct Location ownership", async () => {
    const location = { directory: "/project/shared", workspaceID: "workspace-first" };
    const session = { get: () => Effect.succeed({ location }) };
    expect(await getSessionDirectoryForEvent({ location, session }, "ses_workspace", "/cwd")).toBe(
      "/project/shared",
    );
    expect(
      await getSessionDirectoryForEvent(
        { location: { ...location, workspaceID: "workspace-second" }, session },
        "ses_workspace",
        "/cwd",
      ),
    ).toBeNull();
  });

  test("V1 accepts the stored session directory even when it differs from the instance cwd", async () => {
    const client = { session: { get: async () => ({ data: { directory: "/session/project" } }) } };
    expect(await getSessionDirectoryForEvent(client, "ses_resumed", "/instance/cwd")).toBe(
      "/session/project",
    );
  });

  test("V1 also leaves an unresolved session unassigned", async () => {
    const client = { session: { get: async () => undefined } };
    expect(await getSessionDirectoryForEvent(client, "ses_unknown", "/instance/cwd")).toBeNull();
  });
});

describe("deleted session event directories", () => {
  test("the event Location overrides stale cached ownership without querying the deleted record", async () => {
    await getSessionDirectory(
      {
        location: { directory: "/old" },
        session: { get: () => Effect.succeed({ location: { directory: "/old" } }) },
      },
      "deleted-moved",
      "/old",
    );
    const location = { directory: "/new", workspaceID: "workspace-new" };
    const get = mock(() => Effect.fail(new Error("session removed")));
    const client = { location, session: { get } };
    expect(getSessionDirectoryForDeletedEvent(client, "deleted-moved", location)).toBe("/new");
    expect(getSessionDirectoryCached("deleted-moved")).toBe("/new");
    expect(get).not.toHaveBeenCalled();
  });

  test("a foreign event Location overrides a matching cache rather than falling back to it", async () => {
    const location = { directory: "/own", workspaceID: "own-workspace" };
    const client = { location, session: { get: () => Effect.succeed({ location }) } };
    await getSessionDirectory(client, "deleted-foreign", "/own");
    expect(
      getSessionDirectoryForDeletedEvent(client, "deleted-foreign", {
        directory: "/own",
        workspaceID: "foreign-workspace",
      }),
    ).toBeNull();
    expect(getSessionDirectoryForDeletedEvent(client, "deleted-foreign", undefined)).toBeNull();
  });

  test("a location-less deletion can use its previously resolved Location without a new lookup", async () => {
    const location = { directory: "/own", workspaceID: "workspace" };
    const get = mock(() => Effect.succeed({ location }));
    const client = { location, session: { get } };
    await getSessionDirectory(client, "deleted-cached", "/own");
    expect(getSessionDirectoryForDeletedEvent(client, "deleted-cached", undefined)).toBe("/own");
    expect(get).toHaveBeenCalledTimes(1);
  });

  test("a location-less deletion with no cached ownership never queries the missing record", () => {
    const get = mock(() => Effect.fail(new Error("session removed")));
    const client = { location: { directory: "/own" }, session: { get } };
    expect(getSessionDirectoryForDeletedEvent(client, "deleted-unknown", undefined)).toBeNull();
    expect(get).not.toHaveBeenCalled();
  });
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

  test("caches transient errors briefly so later calls do not immediately refetch", async () => {
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
    // The fallback is provisional, but repeated tool calls must not hammer the host.
    expect(getSessionDirectoryCached("ses_flaky")).toBeNull();
    expect(await getSessionDirectory(client, "ses_flaky", "/cwd")).toBeNull();
    expect(calls).toBe(1);
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
    await waitForCachedSession("ses_warm", "/warmed");
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
    await Promise.resolve();
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

describe("verifySessionDirectory (cross-project serve gate)", () => {
  test("returns the SDK-confirmed directory", async () => {
    const client = {
      session: {
        get: async () => ({ data: { directory: "/real/project" } }),
      },
    };
    expect(await verifySessionDirectory(client, "ses_v1")).toBe("/real/project");
  });

  test("returns null on SDK failure and does NOT memoize the failure", async () => {
    let calls = 0;
    const client = {
      session: {
        get: async () => {
          calls++;
          if (calls === 1) throw new Error("transient");
          return { data: { directory: "/recovered" } };
        },
      },
    };
    expect(await verifySessionDirectory(client, "ses_v2")).toBeNull();
    // Next poll retries instead of serving a memoized failure.
    expect(await verifySessionDirectory(client, "ses_v2")).toBe("/recovered");
    expect(calls).toBe(2);
  });

  test("does not trust the warm cache: poisoned cached entry is ignored", async () => {
    // Seed the long-lived cache with a WRONG mapping (the poisoning scenario:
    // a fallback-seeded warm in a multi-project host).
    const poisoner = {
      session: { get: async () => ({ data: { directory: "/wrong/project" } }) },
    };
    await getSessionDirectory(poisoner, "ses_v3", "/cwd");
    expect(getSessionDirectoryCached("ses_v3")).toBe("/wrong/project");

    // verify must do a FRESH lookup, not read the poisoned cache.
    const truth = {
      session: { get: async () => ({ data: { directory: "/right/project" } }) },
    };
    expect(await verifySessionDirectory(truth, "ses_v3")).toBe("/right/project");
    // And it heals the long-lived cache with the SDK truth.
    expect(getSessionDirectoryCached("ses_v3")).toBe("/right/project");
  });

  test("memoizes successful verification within the TTL", async () => {
    let calls = 0;
    const client = {
      session: {
        get: async () => {
          calls++;
          return { data: { directory: "/memo" } };
        },
      },
    };
    await verifySessionDirectory(client, "ses_v4");
    await verifySessionDirectory(client, "ses_v4");
    expect(calls).toBe(1);
  });

  test("returns null for empty session id and clients without session.get", async () => {
    expect(await verifySessionDirectory({}, "")).toBeNull();
    expect(await verifySessionDirectory({}, "ses_v5")).toBeNull();
  });
});

async function waitForCachedSession(
  sessionID: string,
  expected: string,
  timeoutMs = 1_000,
): Promise<void> {
  const started = Date.now();
  while (getSessionDirectoryCached(sessionID) !== expected) {
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timed out waiting for cached session ${sessionID}`);
    }
    await Promise.resolve();
  }
}
