/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import { Effect, Exit, Schema, Scope } from "effect";

import { makeServerEffect } from "../entry/server-runtime.mjs";
import * as logger from "../logger.js";
import { buildAftToolDefinitions } from "../tool-registration.js";
import { setupV2Tui } from "../tui/v2.js";
import {
  __resetHostToolOverlapNoticeForTests,
  registerV2HostToolOverlapNotice,
} from "../v2-host-tool-overlap.js";

type HookCallback = (event: any) => Effect.Effect<unknown>;
type RpcEventHandler = (event: { type: string; data: unknown }) => void | Promise<void>;
type EventSchemas = Record<string, { schema: Schema.Top }>;

/**
 * The host process's RPC event bus, shared by every Location's server plugin
 * and every TUI client. It is no more capable than the real one: an emitted
 * payload is decoded against the event's schema, events are ephemeral (a
 * client that subscribes later gets nothing), and a client hears only events
 * of the definition it asked for by name.
 */
function fakeRpcBus() {
  const subscribers = new Map<string, Set<RpcEventHandler>>();
  const deliveries: Array<{ type: string; data: unknown }> = [];
  return {
    deliveries,
    register(definition: { id: string; events: EventSchemas }) {
      return Effect.sync(() => ({
        dispose: Effect.void,
        events: {
          emit: (name: string, data: unknown) =>
            Effect.gen(function* () {
              const event = definition.events[name];
              if (!event) return yield* Effect.fail(new Error(`unknown RPC event ${name}`));
              const decoded = yield* Effect.try(() =>
                Schema.decodeUnknownSync(event.schema as never)(data),
              );
              const type = `rpc.${definition.id}.${name}`;
              for (const handler of subscribers.get(type) ?? []) {
                deliveries.push({ type, data: decoded });
                yield* Effect.promise(async () => handler({ type, data: decoded }));
              }
            }),
        },
      }));
    },
    client(definition: { id: string }) {
      return {
        getStatus: async () => ({ success: true }),
        events: {
          on(name: string, handler: RpcEventHandler, options?: { signal?: AbortSignal }) {
            const type = `rpc.${definition.id}.${name}`;
            let set = subscribers.get(type);
            if (!set) {
              set = new Set();
              subscribers.set(type, set);
            }
            set.add(handler);
            const remove = () => set?.delete(handler);
            options?.signal?.addEventListener("abort", remove);
            return remove;
          },
        },
      };
    },
  };
}

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
 * A stand-in for an OpenCode 2 server-plugin Location context, offering only
 * what the real `@opencode/plugin` Context offers: a tool domain whose
 * `list()` returns the registry after every transform (2.0.11; absent on
 * 2.0.3), a session domain with `hook` and `synthetic`, and the `rpc` domain.
 * It has no toast or dialog: a server plugin has none. The registry is fed by
 * the host's own tools and AFT's transform, so `list()` can never report a
 * tool nobody registered. `synthetic` records every call so a test can prove
 * nothing went into the session, which the model reads.
 */
function fakeHost(bus: ReturnType<typeof fakeRpcBus>, options: FakeHostOptions = {}) {
  const promptHooks: HookCallback[] = [];
  const registry = new Map<string, { id: string; name: string }>();
  for (const name of options.hostTools ?? []) registry.set(name, { id: name, name });
  const synthetic: unknown[] = [];
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
      synthetic: (input: unknown) =>
        Effect.sync(() => {
          synthetic.push(input);
        }),
      hook: (name: string, callback: HookCallback) =>
        Effect.sync(() => {
          if (name === "prompt") promptHooks.push(callback);
        }),
    },
    tool,
    rpc: { register: bus.register },
    provider: { list: () => Effect.succeed({ data: [] }) },
  };
  return { context, promptHooks, synthetic, registry };
}

/**
 * A stand-in for the OpenCode 2 TUI plugin context: the RPC client, the toast
 * surface and the few UI members `setupV2Tui` touches. Slots are recorded but
 * never rendered.
 */
function fakeTui(bus: ReturnType<typeof fakeRpcBus>) {
  const toasts: Array<Record<string, unknown>> = [];
  const context = {
    location: { directory: "/work/project" },
    client: { rpc: (definition: { id: string }) => bus.client(definition) },
    keymap: { layer: () => {} },
    ui: {
      dialog: { alert: async () => {} },
      toast: { show: (input: Record<string, unknown>) => toasts.push(input) },
      router: { current: () => ({ type: "session", sessionID: "session-a" }) },
      slot: () => () => {},
    },
  };
  return { context, toasts };
}

const pendingChecks: Promise<void>[] = [];
const openScopes: Scope.Closeable[] = [];

function serverDependencies(config: Record<string, unknown>, extraAftTools: string[] = []) {
  const bridge = {
    toolCall: async () => ({ success: true, text: "ok" }),
    send: async () => ({ success: true }),
  };
  const pool = {
    setConfigureOverride: () => {},
    getBridge: () => bridge,
    toolCall: async () => ({ success: true, text: "ok" }),
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
    startLiveConfigReload: () => ({ stop: () => {} }),
    // The real tool map, plus any extra AFT tool a test names (registered with
    // the apply_patch definition), so a test can make AFT itself own a name.
    buildToolMap: (...args: Parameters<typeof buildAftToolDefinitions>) => {
      const tools = buildAftToolDefinitions(...args);
      for (const name of extraAftTools) {
        const template = tools.apply_patch ?? Object.values(tools)[0];
        if (template) tools[name] = template;
      }
      return tools;
    },
    // The production registration with the production delivery argument, plus
    // a handle on its background check so a test can wait for it.
    registerHostToolOverlapNotice: (
      context: unknown,
      tools: ReadonlySet<string>,
      deliver: (message: string) => Promise<void>,
    ) =>
      registerV2HostToolOverlapNotice(context, tools, deliver, (done) => pendingChecks.push(done)),
  };
}

/**
 * Boot the real V2 server effect (real RPC registration included) against a
 * fake host Location. The scope stays open, as the host keeps a Location's
 * scope open until it disposes the Location, so RPC events still flow.
 */
async function boot(
  host: ReturnType<typeof fakeHost>,
  config: Record<string, unknown> = {},
  extraAftTools: string[] = [],
) {
  const scope = await Effect.runPromise(Scope.make());
  openScopes.push(scope);
  await Effect.runPromise(
    Scope.provide(scope)(
      makeServerEffect(serverDependencies(config, extraAftTools))(host.context as never),
    ),
  );
}

/** Send a user prompt through every registered prompt hook, then let checks finish. */
async function prompt(host: ReturnType<typeof fakeHost>, sessionID: string, metadata?: object) {
  for (const hook of host.promptHooks) {
    await Effect.runPromise(hook({ sessionID, prompt: { text: "hi" }, metadata }));
  }
  await Promise.all(pendingChecks.splice(0));
}

const PATCH_NOTICE =
  "OpenCode's built-in patch tool is still enabled beside AFT's apply_patch, so the model sees two editing tools. Run `npx @cortexkit/aft doctor --fix` to turn the built-in one off.";
const SHELL_NOTICE =
  "OpenCode's built-in shell tool is still enabled beside AFT's bash, so the model sees two shells. Run `npx @cortexkit/aft doctor --fix` to turn the built-in one off.";
const BOTH_NOTICE =
  "OpenCode's built-in patch and shell tools are still enabled beside AFT's apply_patch and bash, so the model sees two editing tools and two shells. Run `npx @cortexkit/aft doctor --fix` to turn the built-in ones off.";

function toast(message: string) {
  return { title: "AFT", message, variant: "warning", duration: 15_000 };
}

let bus: ReturnType<typeof fakeRpcBus>;
let tui: ReturnType<typeof fakeTui>;
let stopTui: (() => void) | undefined;
let hosts: Array<ReturnType<typeof fakeHost>>;

/** A fresh host Location on this test's bus. */
function host(options: FakeHostOptions = {}) {
  const created = fakeHost(bus, options);
  hosts.push(created);
  return created;
}

beforeEach(async () => {
  __resetHostToolOverlapNoticeForTests();
  pendingChecks.length = 0;
  hosts = [];
  bus = fakeRpcBus();
  tui = fakeTui(bus);
  stopTui = await setupV2Tui(tui.context as never);
});

afterEach(async () => {
  // Whatever happened, nothing reached a session: the model reads sessions.
  for (const created of hosts) expect(created.synthetic).toEqual([]);
  stopTui?.();
  for (const scope of openScopes.splice(0)) {
    await Effect.runPromise(Scope.close(scope, Exit.void));
  }
  __resetHostToolOverlapNoticeForTests();
});

describe("OpenCode 2 built-in tool overlap notice", () => {
  test("host patch beside a registered apply_patch shows the patch toast in the TUI", async () => {
    const location = host({ hostTools: ["patch"] });
    await boot(location);
    await prompt(location, "session-a");
    expect(tui.toasts).toEqual([toast(PATCH_NOTICE)]);
    expect(bus.deliveries).toEqual([
      { type: "rpc.aft.hostToolOverlap", data: { message: PATCH_NOTICE } },
    ]);
    expect(location.synthetic).toEqual([]);
  });

  test("host shell beside a registered bash shows the shell toast", async () => {
    const location = host({ hostTools: ["shell"] });
    await boot(location);
    await prompt(location, "session-a");
    expect(tui.toasts).toEqual([toast(SHELL_NOTICE)]);
  });

  test("both built-ins present give one combined toast", async () => {
    const location = host({ hostTools: ["patch", "shell", "glob"] });
    await boot(location);
    await prompt(location, "session-a");
    expect(tui.toasts).toEqual([toast(BOTH_NOTICE)]);
  });

  test("no toast when the host plugins are disabled, so neither built-in is registered", async () => {
    const location = host({ hostTools: ["glob", "grep", "webfetch"] });
    await boot(location);
    await prompt(location, "session-a");
    expect(tui.toasts).toEqual([]);
  });

  test("no toast when apply_patch and bash are in disabled_tools", async () => {
    const location = host({ hostTools: ["patch", "shell"] });
    await boot(location, { disabled_tools: ["apply_patch", "bash"] });
    await prompt(location, "session-a");
    expect(tui.toasts).toEqual([]);
  });

  test("a disabled apply_patch leaves only the shell overlap in the toast", async () => {
    const location = host({ hostTools: ["patch", "shell"] });
    await boot(location, { disabled_tools: ["apply_patch"] });
    await prompt(location, "session-a");
    expect(tui.toasts).toEqual([toast(SHELL_NOTICE)]);
  });

  test("a patch tool AFT registered itself is not taken for the host's built-in", async () => {
    // The host registers no patch of its own; the only `patch` in the
    // registry is the one AFT's transform added.
    const location = host({ hostTools: ["glob"] });
    await boot(location, {}, ["patch"]);
    expect(location.registry.has("patch")).toBe(true);
    await prompt(location, "session-a");
    expect(tui.toasts).toEqual([]);
  });

  test("shown at most once across several sessions and Locations in one process", async () => {
    const first = host({ hostTools: ["patch"], directory: "/work/one" });
    const second = host({ hostTools: ["patch"], directory: "/work/two" });
    await boot(first);
    await boot(second);
    await prompt(first, "session-a");
    await prompt(first, "session-b");
    await prompt(second, "session-c");
    await prompt(first, "session-a");
    expect(tui.toasts).toEqual([toast(PATCH_NOTICE)]);
  });

  test("a completion wake AFT admitted itself does not trigger the check", async () => {
    const location = host({ hostTools: ["patch"] });
    await boot(location);
    await prompt(location, "session-wake", { source: "aft" });
    expect(tui.toasts).toEqual([]);
    await prompt(location, "session-user");
    expect(tui.toasts).toEqual([toast(PATCH_NOTICE)]);
  });

  test("a TUI that subscribes after the notice went out does not get it replayed", async () => {
    // RPC events are ephemeral on the real host; the fake must not replay them.
    stopTui?.();
    const location = host({ hostTools: ["patch"] });
    await boot(location);
    await prompt(location, "session-a");
    stopTui = await setupV2Tui(tui.context as never);
    expect(tui.toasts).toEqual([]);
    expect(bus.deliveries).toEqual([]);
  });

  test("a host without tool.list() stays silent and logs why once at debug level", async () => {
    const debugSpy = spyOn(logger, "debug");
    try {
      const first = host({ hostTools: ["patch", "shell"], hasToolList: false });
      const second = host({ hostTools: ["patch"], hasToolList: false });
      await boot(first);
      await boot(second);
      await prompt(first, "session-a");
      await prompt(first, "session-b");
      await prompt(second, "session-c");
      expect(tui.toasts).toEqual([]);
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
      const location = host({ hostTools: ["patch"], listFails: true });
      await boot(location);
      await prompt(location, "session-a");
      expect(tui.toasts).toEqual([]);
      const reasons = debugSpy.mock.calls
        .map((call) => String(call[0]))
        .filter((message) => message.includes("tool.list() failed"));
      expect(reasons).toHaveLength(1);
    } finally {
      debugSpy.mockRestore();
    }
  });

  test("an OpenCode 1 host context gets no prompt hook and no notice", async () => {
    // A V1 host's bundled core loader can call the V2 effect with a context
    // that has no Location; the runtime must not register anything on it.
    const location = host({ hostTools: ["patch", "shell"] });
    const v1Context = { ...location.context, location: undefined };
    await Effect.runPromise(
      Effect.scoped(makeServerEffect(serverDependencies({}))(v1Context as never)),
    );
    expect(location.promptHooks).toEqual([]);
    expect(tui.toasts).toEqual([]);
  });
});
