/// <reference path="../bun-test.d.ts" />
/**
 * Issue #387: on OpenCode 2, a session created in a linked git worktree must
 * resolve tool paths and the bash working directory against that worktree,
 * not against the project's main checkout.
 *
 * The OpenCode 2 host hands AFT its plugin context where OpenCode 1 hands the
 * SDK client. Its `session.get` takes `{ sessionID }`, returns an Effect, and
 * the session record carries its directory at `location.directory`. The tool
 * runtime's `worktree` is the main checkout (the Location's
 * `project.canonical`), so any path that falls back to it lands in the wrong
 * checkout.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { Effect } from "effect";

import {
  _resetSessionDirectoryCacheForTest,
  getSessionDirectory,
  getSessionDirectoryCached,
  V2_SESSION_LOOKUP_TIMEOUT_MS,
} from "../shared/session-directory.js";
import { callBridge, projectRootFor, resolveProjectRoot } from "../tools/_shared.js";
import { projectV2Tool } from "../tools/definitions/v2.js";

const tempRoots: string[] = [];

afterEach(() => {
  _resetSessionDirectoryCacheForTest();
  for (const root of tempRoots.splice(0)) rmSync(root, { recursive: true, force: true });
});

/** A main checkout and a linked worktree that lives outside it, as real directories. */
function checkoutLayout(): { main: string; worktree: string } {
  const root = realpathSync(mkdtempSync(path.join(tmpdir(), "aft-v2-session-root-")));
  tempRoots.push(root);
  const main = path.join(root, "main");
  const worktree = path.join(root, "linked-worktree");
  mkdirSync(main);
  mkdirSync(worktree);
  return { main, worktree };
}

type SessionGet = (input: { sessionID: string }) => Effect.Effect<unknown, unknown>;

/** The OpenCode 2 plugin context as AFT receives it in `ctx.client`. */
function v2Context(location: { directory: string }, get: SessionGet) {
  return { location, session: { get } };
}

describe("OpenCode 2 session directory lookup", () => {
  test("calls session.get({ sessionID }) and reads location.directory from the Effect", async () => {
    const inputs: unknown[] = [];
    const context = v2Context({ directory: "/work/linked" }, (input) => {
      inputs.push(input);
      return Effect.succeed({
        id: input.sessionID,
        location: { directory: "/work/linked" },
      });
    });

    expect(await getSessionDirectory(context, "ses_v2", "/work/linked")).toBe("/work/linked");
    expect(inputs).toEqual([{ sessionID: "ses_v2" }]);
    expect(getSessionDirectoryCached("ses_v2")).toBe("/work/linked");
  });

  test("a failed lookup is cached as null and not immediately retried", async () => {
    let calls = 0;
    const context = v2Context({ directory: "/work/linked" }, () => {
      calls++;
      return Effect.fail(new Error("session store unavailable"));
    });

    expect(await getSessionDirectory(context, "ses_fail", "/work/linked")).toBeNull();
    expect(getSessionDirectoryCached("ses_fail")).toBeNull();
    expect(await getSessionDirectory(context, "ses_fail", "/work/linked")).toBeNull();
    expect(calls).toBe(1);
  });

  test(
    "a lookup that never answers gives up after the timeout",
    async () => {
      const context = v2Context({ directory: "/work/linked" }, () => Effect.never);
      const started = Date.now();
      expect(await getSessionDirectory(context, "ses_hang", "/work/linked")).toBeNull();
      expect(Date.now() - started).toBeGreaterThanOrEqual(V2_SESSION_LOOKUP_TIMEOUT_MS - 50);
      expect(Date.now() - started).toBeLessThan(1_500);
      expect(getSessionDirectoryCached("ses_hang")).toBeNull();
    },
    V2_SESSION_LOOKUP_TIMEOUT_MS + 5_000,
  );

  test("the OpenCode 1 SDK shape is still called with path.id and reads directory", async () => {
    const inputs: unknown[] = [];
    const client = {
      session: {
        get: async (input: { path: { id: string } }) => {
          inputs.push(input);
          return { data: { directory: "/v1/project" } };
        },
      },
    };
    expect(await getSessionDirectory(client, "ses_v1", "/cwd")).toBe("/v1/project");
    expect(inputs).toEqual([{ path: { id: "ses_v1" } }]);
  });
});

describe("OpenCode 2 tool root for a session in a linked worktree", () => {
  let lastSessionID = "";

  test("projectRootFor uses directory, not worktree, for an OpenCode 2 runtime with no cached lookup", () => {
    const { main, worktree } = checkoutLayout();
    expect(
      projectRootFor({ directory: worktree, worktree: main, directoryIsSessionRoot: true }),
    ).toBe(worktree);
    // OpenCode 1 runtimes keep preferring worktree.
    expect(projectRootFor({ directory: worktree, worktree: main })).toBe(main);
  });

  test("OpenCode 2 follows a new per-call directory while OpenCode 1 keeps the cached directory", async () => {
    const { main, worktree } = checkoutLayout();
    const sessionID = "ses_moved";
    await getSessionDirectory(
      v2Context({ directory: main }, () => Effect.succeed({ location: { directory: main } })),
      sessionID,
      main,
    );
    expect(getSessionDirectoryCached(sessionID)).toBe(main);

    const runtime = { directory: worktree, worktree: main, sessionID };
    expect(projectRootFor({ ...runtime, directoryIsSessionRoot: true })).toBe(worktree);
    expect(projectRootFor(runtime)).toBe(main);
    expect(projectRootFor({ ...runtime, directory: "", directoryIsSessionRoot: true })).toBe(main);
  });

  /**
   * Run one projected V2 tool whose body routes a bridge call the way every
   * AFT tool does, and report the root it resolved and the bridge root it hit.
   * The bridge root is what the Rust side uses to resolve relative paths and
   * as the bash working directory.
   */
  async function runProjectedTool(
    sessionGet: (layout: { main: string; worktree: string }) => SessionGet,
  ): Promise<{
    layout: { main: string; worktree: string };
    resolved: string;
    bridgeRoots: string[];
  }> {
    const layout = checkoutLayout();
    const location = {
      directory: layout.worktree,
      project: { directory: layout.worktree, canonical: layout.main },
    };
    const bridgeRoots: string[] = [];
    const ctx = {
      pool: {
        getBridge: (root: string) => {
          bridgeRoots.push(root);
          return { send: async () => ({ success: true }) };
        },
      },
      client: v2Context(location, sessionGet(layout)),
      config: {},
      storageDir: "/isolated/storage",
    } as never;
    let resolved = "";
    const definition: ToolDefinition = {
      description: "root probe",
      args: {},
      execute: async (_input, runtime) => {
        resolved = await resolveProjectRoot(ctx, runtime);
        await callBridge(ctx, runtime, "read", { file: "only-in-worktree.txt" });
        return "ok";
      },
    };
    const tool = projectV2Tool("read", definition, location);
    lastSessionID = `ses_${Math.random().toString(36).slice(2)}`;
    await Effect.runPromise(
      tool.execute({}, { sessionID: lastSessionID, progress: () => Effect.succeed(undefined) }),
    );
    return { layout, resolved, bridgeRoots };
  }

  test("a session placed in the worktree routes to the worktree, not the main checkout", async () => {
    const { layout, resolved, bridgeRoots } = await runProjectedTool(
      (layout) => () => Effect.succeed({ location: { directory: layout.worktree } }),
    );
    expect(getSessionDirectoryCached(lastSessionID)).toBe(layout.worktree);
    expect(resolved).toBe(layout.worktree);
    expect(bridgeRoots).toEqual([layout.worktree]);
  });

  test("a failed session lookup falls back to the Location directory, never the main checkout", async () => {
    const { layout, resolved, bridgeRoots } = await runProjectedTool(
      () => () => Effect.fail(new Error("session store unavailable")),
    );
    expect(getSessionDirectoryCached(lastSessionID)).toBeNull();
    expect(resolved).toBe(layout.worktree);
    expect(bridgeRoots).toEqual([layout.worktree]);
  });
});
