/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
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
