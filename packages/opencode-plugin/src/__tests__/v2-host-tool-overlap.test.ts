/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import { Effect } from "effect";

import { makeServerEffect } from "../entry/server-runtime.mjs";
import * as logger from "../logger.js";
import {
  __resetHostToolOverlapNoticeForTests,
  registerV2HostToolOverlapNotice,
} from "../v2-host-tool-overlap.js";

type HookCallback = (event: any) => Effect.Effect<unknown>;

interface FakeHostOptions {
  /** Tools the host's own plugins registered (a disabled host plugin registers none). */
  hostTools?: string[];
  /** false: a host whose ToolDomain has no `list()`, as on OpenCode 2.0.3. */
  hasToolList?: boolean;
  /** Make `tool.list()` fail the way an Effect fails. */
  listFails?: boolean;
  directory?: string;
}

/**
 * A stand-in for an OpenCode 2 host Location context, offering only what the
 * real `@opencode/plugin` Context offers: a tool domain whose `list()` returns
 * the registry after every transform (2.0.11; absent on 2.0.3), a session
 * domain with `hook` and `synthetic`, and nothing else the check could read.
 * The registry is fed by the host's own tools and AFT's transform, so `list()`
 * can never report a tool nobody registered.
 */
function fakeHost(options: FakeHostOptions = {}) {
  const promptHooks: HookCallback[] = [];
  const registry = new Map<string, { id: string; name: string }>();
  for (const name of options.hostTools ?? []) registry.set(name, { id: name, name });
  const synthetic: Array<{ sessionID: string; text: string; resume: boolean }> = [];
  const tool: Record<string, unknown> = {
    hook: () => Effect.void,
    transform: (register: (editor: any) => void) =>
      Effect.sync(() =>
        register({
          add: (definition: { name: string }) =>
            registry.set(definition.name, { id: definition.name, name: definition.name }),
          remove: (id: string) => registry.delete(id),
        }),
      ),
  };
  if (options.hasToolList !== false) {
    tool.list = () =>
      options.listFails
        ? Effect.fail(new Error("tool registry unavailable"))
        : Effect.sync(() => [...registry.values()]);
  }
  const context = {
    location: { directory: options.directory ?? "/work/project" },
    session: {
      get: () => Effect.succeed({}),
      prompt: () => Effect.die("a notice must never prompt the model"),
      synthetic: (input: { sessionID: string; text: string; resume: boolean }) =>
        Effect.sync(() => {
          synthetic.push(input);
        }),
      hook: (name: string, callback: HookCallback) =>
        Effect.sync(() => {
          if (name === "prompt") promptHooks.push(callback);
        }),
    },
    tool,
    provider: { list: () => Effect.succeed({ data: [] }) },
  };
  return { context, promptHooks, synthetic };
}

const pendingChecks: Promise<void>[] = [];

function serverDependencies(config: Record<string, unknown>) {
  const bridge = {
    toolCall: async () => ({ success: true, text: "ok" }),
    send: async () => ({ success: true }),
  };
  const pool = {
    setConfigureOverride: () => {},
    getBridge: () => bridge,
    getActiveBridgeForRoot: () => bridge,
    activeBridges: () => [bridge],
  };
  return {
    loadConfig: () => ({
      disabled_tools: [],
      indexes: { lexical: true, semantic: false, callgraph: true },
      ...config,
    }),
    configLoadErrors: () => [],
    configLoadSources: () => [],
    configLoadTexts: () => new Map(),
    deliverLoadNotices: () => {},
    migrateConfigLocations: () => [],
    ensureStorageMigrated: async () => {},
    ensureOnnxRuntime: async () => null,
    startLspAutoInstall: () => null,
    pushLspPaths: async () => {},
    resolveStorageRoot: () => "/isolated/storage",
    buildConfigureParams: () => ({}),
    resolveVersion: () => "0.58.2",
    resolveBinary: async () => "/isolated/aft",
    acquireBridge: async () => pool,
    releaseBridge: async () => {},
    registerRpc: () => Effect.succeed({ dispose: async () => {} }),
    startLiveConfigReload: () => ({ stop: () => {} }),
    // The production registration, with a handle on its background check so a
    // test can wait for it instead of sleeping.
    registerHostToolOverlapNotice: (context: unknown, tools: ReadonlySet<string>) =>
      registerV2HostToolOverlapNotice(context, tools, (done) => pendingChecks.push(done)),
  };
}

/** Boot the real V2 server effect against a fake host Location. */
async function boot(host: ReturnType<typeof fakeHost>, config: Record<string, unknown> = {}) {
  await Effect.runPromise(
    Effect.scoped(makeServerEffect(serverDependencies(config))(host.context as never)),
  );
}

/** Send a user prompt through every registered prompt hook, then let checks finish. */
async function prompt(host: ReturnType<typeof fakeHost>, sessionID: string) {
  for (const hook of host.promptHooks) {
    await Effect.runPromise(hook({ sessionID, prompt: { text: "hi" } }));
  }
  await Promise.all(pendingChecks.splice(0));
}

const PATCH_NOTICE =
  "🔧 AFT: OpenCode's built-in patch tool is still enabled beside AFT's apply_patch, so the model sees two editing tools. Run `npx @cortexkit/aft doctor --fix` to turn the built-in one off.";
const SHELL_NOTICE =
  "🔧 AFT: OpenCode's built-in shell tool is still enabled beside AFT's bash, so the model sees two shells. Run `npx @cortexkit/aft doctor --fix` to turn the built-in one off.";
const BOTH_NOTICE =
  "🔧 AFT: OpenCode's built-in patch and shell tools are still enabled beside AFT's apply_patch and bash, so the model sees two editing tools and two shells. Run `npx @cortexkit/aft doctor --fix` to turn the built-in ones off.";

beforeEach(() => {
  __resetHostToolOverlapNoticeForTests();
  pendingChecks.length = 0;
});

afterEach(() => {
  __resetHostToolOverlapNoticeForTests();
});

describe("OpenCode 2 built-in tool overlap notice", () => {
  test("host patch beside a registered apply_patch shows the patch notice in the chat", async () => {
    const host = fakeHost({ hostTools: ["patch"] });
    await boot(host);
    await prompt(host, "session-a");
    expect(host.synthetic).toEqual([{ sessionID: "session-a", text: PATCH_NOTICE, resume: false }]);
  });

  test("host shell beside a registered bash shows the shell notice", async () => {
    const host = fakeHost({ hostTools: ["shell"] });
    await boot(host);
    await prompt(host, "session-a");
    expect(host.synthetic).toEqual([{ sessionID: "session-a", text: SHELL_NOTICE, resume: false }]);
  });

  test("both built-ins present give one combined notice", async () => {
    const host = fakeHost({ hostTools: ["patch", "shell", "glob"] });
    await boot(host);
    await prompt(host, "session-a");
    expect(host.synthetic).toEqual([{ sessionID: "session-a", text: BOTH_NOTICE, resume: false }]);
  });

  test("no notice when the host plugins are disabled, so neither built-in is registered", async () => {
    const host = fakeHost({ hostTools: ["glob", "grep", "webfetch"] });
    await boot(host);
    await prompt(host, "session-a");
    expect(host.synthetic).toEqual([]);
  });

  test("no notice when apply_patch and bash are in disabled_tools", async () => {
    const host = fakeHost({ hostTools: ["patch", "shell"] });
    await boot(host, { disabled_tools: ["apply_patch", "bash"] });
    await prompt(host, "session-a");
    expect(host.synthetic).toEqual([]);
  });

  test("a disabled apply_patch leaves only the shell overlap in the notice", async () => {
    const host = fakeHost({ hostTools: ["patch", "shell"] });
    await boot(host, { disabled_tools: ["apply_patch"] });
    await prompt(host, "session-a");
    expect(host.synthetic).toEqual([{ sessionID: "session-a", text: SHELL_NOTICE, resume: false }]);
  });

  test("shown at most once across several sessions and Locations in one process", async () => {
    const first = fakeHost({ hostTools: ["patch"], directory: "/work/one" });
    const second = fakeHost({ hostTools: ["patch"], directory: "/work/two" });
    await boot(first);
    await boot(second);
    await prompt(first, "session-a");
    await prompt(first, "session-b");
    await prompt(second, "session-c");
    await prompt(first, "session-a");
    expect(first.synthetic).toEqual([
      { sessionID: "session-a", text: PATCH_NOTICE, resume: false },
    ]);
    expect(second.synthetic).toEqual([]);
  });

  test("a completion wake AFT admitted itself does not carry the notice", async () => {
    const host = fakeHost({ hostTools: ["patch"] });
    await boot(host);
    for (const hook of host.promptHooks) {
      await Effect.runPromise(
        hook({ sessionID: "session-wake", prompt: { text: "done" }, metadata: { source: "aft" } }),
      );
    }
    await Promise.all(pendingChecks.splice(0));
    expect(host.synthetic).toEqual([]);
    await prompt(host, "session-user");
    expect(host.synthetic).toEqual([
      { sessionID: "session-user", text: PATCH_NOTICE, resume: false },
    ]);
  });

  test("a host without tool.list() stays silent and logs why once at debug level", async () => {
    const debugSpy = spyOn(logger, "debug");
    try {
      const first = fakeHost({ hostTools: ["patch", "shell"], hasToolList: false });
      const second = fakeHost({ hostTools: ["patch"], hasToolList: false });
      await boot(first);
      await boot(second);
      await prompt(first, "session-a");
      await prompt(first, "session-b");
      await prompt(second, "session-c");
      expect(first.synthetic).toEqual([]);
      expect(second.synthetic).toEqual([]);
      // Only this reason is counted: a server effect booted by another test
      // file in the same process may log its own, unrelated reason.
      const reasons = debugSpy.mock.calls
        .map((call) => String(call[0]))
        .filter((message) => message.includes("no tool.list()"));
      expect(reasons).toEqual([
        "Built-in tool overlap check skipped: the host context has no tool.list(); no notice shown",
      ]);
    } finally {
      debugSpy.mockRestore();
    }
  });

  test("a failing tool.list() stays silent and logs the failure at debug level", async () => {
    const debugSpy = spyOn(logger, "debug");
    try {
      const host = fakeHost({ hostTools: ["patch"], listFails: true });
      await boot(host);
      await prompt(host, "session-a");
      expect(host.synthetic).toEqual([]);
      const reasons = debugSpy.mock.calls
        .map((call) => String(call[0]))
        .filter((message) => message.includes("tool.list() failed"));
      expect(reasons).toHaveLength(1);
      expect(reasons[0]).toContain("tool.list() failed");
    } finally {
      debugSpy.mockRestore();
    }
  });

  test("an OpenCode 1 host context gets no prompt hook and no notice", async () => {
    // A V1 host's bundled core loader can call the V2 effect with a context
    // that has no Location; the runtime must not register anything on it.
    const host = fakeHost({ hostTools: ["patch", "shell"] });
    const v1Context = { ...host.context, location: undefined };
    await Effect.runPromise(
      Effect.scoped(makeServerEffect(serverDependencies({}))(v1Context as never)),
    );
    expect(host.promptHooks).toEqual([]);
    expect(host.synthetic).toEqual([]);
  });
});
