/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { Effect } from "effect";
import { _resetSubagentCacheForTest, resolveIsSubagent } from "../shared/subagent-detect.js";

afterEach(() => {
  _resetSubagentCacheForTest();
});

describe("subagent-detect", () => {
  test("returns false when sessionId is empty", async () => {
    const result = await resolveIsSubagent({}, "", "/cwd");
    expect(result).toBe(false);
  });

  test("returns false when sessionId is undefined", async () => {
    const result = await resolveIsSubagent({}, undefined, "/cwd");
    expect(result).toBe(false);
  });

  test("returns false when client lacks session.get", async () => {
    const result = await resolveIsSubagent({}, "ses_foo", "/cwd");
    expect(result).toBe(false);
  });

  test("returns true when SDK returns non-empty parentID", async () => {
    const client = {
      session: {
        get: async (_input: { path: { id: string }; query?: { directory?: string } }) => ({
          data: { id: "ses_child", parentID: "ses_parent" },
        }),
      },
    };
    const result = await resolveIsSubagent(client, "ses_child", "/cwd");
    expect(result).toBe(true);
  });

  test("calls SDK with path: { id } shape (NOT flat sessionID) and omits directory query", async () => {
    // Regression: the SDK schema is `{ path: { id }, query?: { directory } }`.
    // Passing a flat `{ sessionID, directory }` caused the SDK to receive
    // `id = undefined` and return a different session whose parentID was
    // undefined — silently breaking the subagent gate.
    let lastInput: unknown;
    const client = {
      session: {
        get: async (input: unknown) => {
          lastInput = input;
          return { data: { id: "ses_child", parentID: "ses_parent" } };
        },
      },
    };
    await resolveIsSubagent(client, "ses_child", "/some/cwd");
    expect(lastInput).toEqual({ path: { id: "ses_child" } });
    // Specifically: no `directory` should leak through. Looking up a session
    // by ID is an identity query, not a directory-scoped one.
    expect((lastInput as { query?: unknown }).query).toBeUndefined();
    expect((lastInput as { sessionID?: unknown }).sessionID).toBeUndefined();
    expect((lastInput as { directory?: unknown }).directory).toBeUndefined();
  });

  test("returns false when SDK returns empty parentID", async () => {
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => ({
          data: { id: "ses_root", parentID: "" },
        }),
      },
    };
    const result = await resolveIsSubagent(client, "ses_root", "/cwd");
    expect(result).toBe(false);
  });

  test("returns false when SDK returns missing parentID", async () => {
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => ({
          data: { id: "ses_root" }, // no parentID at all
        }),
      },
    };
    const result = await resolveIsSubagent(client, "ses_root", "/cwd");
    expect(result).toBe(false);
  });

  test("handles SDK response shape without `data` wrapper", async () => {
    // ThrowOnError: true variant returns the Session directly, not wrapped.
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => ({
          id: "ses_child",
          parentID: "ses_parent",
        }),
      },
    };
    const result = await resolveIsSubagent(client, "ses_child", "/cwd");
    expect(result).toBe(true);
  });

  test("caches result so second call does not hit SDK", async () => {
    let calls = 0;
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => {
          calls += 1;
          return { data: { id: "ses_x", parentID: "ses_parent" } };
        },
      },
    };
    const first = await resolveIsSubagent(client, "ses_x", "/cwd");
    const second = await resolveIsSubagent(client, "ses_x", "/cwd");
    expect(first).toBe(true);
    expect(second).toBe(true);
    expect(calls).toBe(1);
  });

  test("does not cache on SDK error — next call retries", async () => {
    let calls = 0;
    let shouldThrow = true;
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => {
          calls += 1;
          if (shouldThrow) throw new Error("transient SDK failure");
          return { data: { id: "ses_y", parentID: "ses_parent" } };
        },
      },
    };
    const first = await resolveIsSubagent(client, "ses_y", "/cwd");
    expect(first).toBe(false); // error path defaults to false
    expect(calls).toBe(1);

    // Next call retries because the error wasn't cached
    shouldThrow = false;
    const second = await resolveIsSubagent(client, "ses_y", "/cwd");
    expect(second).toBe(true);
    expect(calls).toBe(2);
  });

  test("preserves `this` binding when calling SDK session.get (regression: this._client)", async () => {
    // Mirrors the real OpenCode SDK shape where Session.get is a class
    // method that depends on `this._client`. Extracting the function
    // reference and calling it without binding crashes with
    // "undefined is not an object (evaluating 'this._client')".
    class FakeSessionApi {
      private readonly _client = { ok: true };
      async get(input: { path: { id: string } }) {
        if (!this._client?.ok)
          throw new Error("undefined is not an object (evaluating 'this._client')");
        return { data: { id: input.path.id, parentID: "ses_parent" } };
      }
    }
    const client = { session: new FakeSessionApi() };
    const result = await resolveIsSubagent(client, "ses_bind", "/cwd");
    expect(result).toBe(true);
  });

  test("returns false when SDK call returns undefined", async () => {
    const client = {
      session: {
        get: async () => undefined,
      },
    };
    const result = await resolveIsSubagent(client, "ses_nil", "/cwd");
    expect(result).toBe(false);
  });

  describe("OpenCode 2 plugin context", () => {
    // The OpenCode 2 runtime hands tools the plugin context itself as
    // `client`. That context has no SDK `client`; it has a `location` and a
    // `session` domain whose `get` takes `{ sessionID }` and returns an Effect
    // resolving to the bare session record. This double carries only those
    // capabilities, so a detector that still speaks the V1 SDK shape cannot
    // pass by accident.
    function v2Context(records: Record<string, { id: string; parentID?: string }>) {
      const inputs: unknown[] = [];
      const context = {
        location: { directory: "/cwd" },
        session: {
          get: (input: { sessionID?: string }) => {
            inputs.push(input);
            const record = input?.sessionID ? records[input.sessionID] : undefined;
            return record
              ? Effect.succeed(record)
              : Effect.fail(new Error(`session ${JSON.stringify(input?.sessionID)} not found`));
          },
        },
      };
      return { context, inputs };
    }

    test("classifies a child session as a subagent", async () => {
      const { context, inputs } = v2Context({
        ses_child: { id: "ses_child", parentID: "ses_parent" },
      });
      expect(await resolveIsSubagent(context, "ses_child", "/cwd")).toBe(true);
      expect(inputs).toEqual([{ sessionID: "ses_child" }]);
    });

    test("classifies a root session as primary", async () => {
      const { context } = v2Context({ ses_root: { id: "ses_root" } });
      expect(await resolveIsSubagent(context, "ses_root", "/cwd")).toBe(false);
    });

    test("a failed lookup is not cached, so the next call can still find the parent", async () => {
      const records: Record<string, { id: string; parentID?: string }> = {};
      const { context, inputs } = v2Context(records);
      expect(await resolveIsSubagent(context, "ses_late", "/cwd")).toBe(false);
      records.ses_late = { id: "ses_late", parentID: "ses_parent" };
      expect(await resolveIsSubagent(context, "ses_late", "/cwd")).toBe(true);
      expect(inputs).toHaveLength(2);
    });
  });

  test("caches negative result (primary session) so repeat calls are O(1)", async () => {
    let calls = 0;
    const client = {
      session: {
        get: async (_input: { path: { id: string } }) => {
          calls += 1;
          return { data: { id: "ses_primary" } }; // no parentID
        },
      },
    };
    await resolveIsSubagent(client, "ses_primary", "/cwd");
    await resolveIsSubagent(client, "ses_primary", "/cwd");
    await resolveIsSubagent(client, "ses_primary", "/cwd");
    expect(calls).toBe(1);
  });
});
