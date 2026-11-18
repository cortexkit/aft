import { afterEach, expect, test } from "bun:test";
import { Effect } from "effect";

import { makeServerEffect } from "../entry/server-runtime.mjs";
import { __resetV2WakeRoutesForTests, createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import { drainEntry } from "./helpers/drain-wire.js";

afterEach(__resetV2WakeRoutesForTests);

test("V2 linked-worktree restart recovers missed matches and completions from the tool root", async () => {
  const main = "/project/main";
  const worktree = "/project/linked";
  const synthetic: Array<{ text: string }> = [];
  const reads: string[] = [];
  let pending = true;
  let completed = true;
  let consumers: ReturnType<typeof createV2RuntimeConsumer> | undefined;
  const match = {
    session_id: "worktree-session",
    task_id: "running",
    watch_id: "ready",
    match_offset: 9,
    match_text: "READY",
    context: "missed worktree match",
    once: true,
  };
  const completion = {
    session_id: "worktree-session",
    task_id: "done",
    status: "completed",
    command: "true",
    exit_code: 0,
    output_preview: "missed worktree completion",
  };
  const bridgeFor = (root: string) => ({
    send: async (command: string, params: Record<string, unknown>) => {
      if (command === "bash_drain_completions") {
        reads.push(root);
        return {
          success: true,
          pending_matches: root === worktree && pending ? [match] : [],
          bg_completions: root === worktree && completed ? [drainEntry(completion)] : [],
        };
      }
      if (command === "bash_ack_completions" && root === worktree) {
        if (params.matches) pending = false;
        if (params.task_ids) completed = false;
        return { success: true, acked_matches: params.matches ?? [] };
      }
      return { success: true };
    },
  });
  const pool = {
    getBridge: bridgeFor,
    setConfigureOverride: () => {},
    getActiveBridgeForRoot: () => null,
    activeBridges: () => [],
  };
  const context = {
    permission: { assert: () => Effect.void },
    location: { directory: worktree, project: { canonical: main } },
    session: {
      get: () => Effect.succeed({}),
      prompt: () => Effect.void,
      synthetic: (input: { text: string }) => Effect.sync(() => synthetic.push(input)),
    },
  };
  await Effect.runPromise(
    Effect.scoped(
      Effect.gen(function* () {
        yield* makeServerEffect({
          loadConfig: () => ({ disabled_tools: [], indexes: { semantic: false } }),
          configLoadErrors: () => [],
          configLoadSources: () => [],
          configLoadTexts: () => new Map(),
          deliverLoadNotices: () => {},
          migrateConfigLocations: () => [],
          ensureOnnxRuntime: async () => null,
          startLspAutoInstall: () => null,
          pushLspPaths: async () => {},
          resolveStorageRoot: () => "/isolated/storage",
          buildConfigureParams: () => ({}),
          resolveVersion: () => "0.0.0-test",
          resolveBinary: async () => "/isolated/aft",
          acquireBridge: async () => pool,
          releaseBridge: async () => {},
          startLiveConfigReload: () => ({ stop: () => {} }),
          registerRpc: () => Effect.succeed({ dispose: async () => {} }),
          registerTools: () => Effect.void,
          registerPromptHook: () => Effect.void,
          registerToolHooks: () => Effect.void,
          registerWorkflowHints: () => Effect.void,
          buildToolMap: () => ({}),
          toolConsumers: (
            host: never,
            _config: unknown,
            getConfig: () => never,
            getBridge: () => never,
          ) => {
            consumers = createV2RuntimeConsumer(host, getConfig, getBridge);
            return consumers;
          },
        })(context);
        yield* Effect.promise(async () => {
          consumers?.registerSession?.("worktree-session");
          await Bun.sleep(0);
          expect(synthetic.map((input) => input.text)).toEqual([
            expect.stringContaining("missed worktree match"),
            expect.stringContaining("missed worktree completion"),
          ]);
          expect(pending).toBe(false);
          expect(completed).toBe(false);
          expect(reads.every((root) => root === worktree)).toBe(true);
        });
      }),
    ),
  );
});

test("V2 recovery keeps the bridge root supplied by a background-event nudge", async () => {
  const roots: Array<string | undefined> = [];
  const consumer = createV2RuntimeConsumer(
    {
      session: {
        get: () => Effect.succeed({}),
        prompt: () => Effect.void,
        synthetic: () => Effect.void,
      },
    },
    () => ({}),
    (root?: string) => {
      roots.push(root);
      return { send: async () => ({ success: true, pending_matches: [] }) } as never;
    },
  );
  try {
    consumer.registerSession?.("s");
    await Bun.sleep(0);
    consumer.transportOptions.onBgEventsNudge?.("/project/nudged-worktree", "s");
    await Bun.sleep(0);
    expect(roots).toEqual([undefined, "/project/nudged-worktree"]);
  } finally {
    consumer.dispose();
  }
});
