/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import { mkdirSync, mkdtempSync, realpathSync, rmSync } from "node:fs";
import { join, resolve } from "node:path";
import * as bridge from "@cortexkit/aft-bridge";
import { Effect, Exit, PubSub, Scope, Stream } from "effect";
import * as notifications from "../bg-notifications.js";
import * as bootstrap from "../bridge-bootstrap.js";
import * as reload from "../config-live-reload.js";
import * as updater from "../hooks/auto-update-checker/index.js";
import { AftRpcServer } from "../shared/rpc-server.js";
import { _resetSessionDirectoryCacheForTest } from "../shared/session-directory.js";
import * as registration from "../tool-registration.js";
import type { PluginContext } from "../types.js";
import { registerV2SessionLifecycle } from "../v2-session-lifecycle.js";

type Plugin = typeof import("../index.js").default;
type Hooks = {
  event(input: { event: { type: string; properties: { sessionID: string } } }): Promise<void>;
  dispose(): Promise<void>;
};
type SubcClient = Awaited<
  ReturnType<NonNullable<ConstructorParameters<typeof bridge.SubcTransportPool>[0]["connect"]>>
>;

let sandbox: string | undefined;
const hooks: Hooks[] = [];
const pools: bridge.SubcTransportPool[] = [];
const scopes: Array<Parameters<typeof Scope.close>[0]> = [];
let importCounter = 0;

afterEach(async () => {
  await Promise.all(
    scopes.splice(0).map((scope) => Effect.runPromise(Scope.close(scope, Exit.void))),
  );
  await Promise.all(hooks.splice(0).map((instance) => instance.dispose()));
  await Promise.all(pools.splice(0).map((pool) => pool.shutdown()));
  mock.restore();
  notifications.__resetBgNotificationStateForTests();
  _resetSessionDirectoryCacheForTest();
  if (sandbox) rmSync(sandbox, { recursive: true, force: true });
  sandbox = undefined;
});

async function setup() {
  const scratch = resolve(import.meta.dir, "../../../../target/scratch");
  mkdirSync(scratch, { recursive: true });
  sandbox = realpathSync(mkdtempSync(join(scratch, "session-events-")));
  const first = join(sandbox, "first");
  const second = join(sandbox, "second");
  mkdirSync(first);
  mkdirSync(second);
  spyOn(bootstrap, "resolveBootstrapConfig").mockResolvedValue({ ok: true, config: {} });
  spyOn(bootstrap, "prepareBridgeEnvironment").mockResolvedValue({
    binaryPath: null,
    storageDir: sandbox,
    configOverrides: { storage_dir: sandbox },
    onnxRuntime: null,
    attach() {},
  });
  spyOn(bootstrap, "createProjectAcceptance").mockReturnValue(() => true);
  spyOn(reload, "startOpenCodeLiveConfigReload").mockReturnValue({ stop() {} });
  spyOn(updater, "createAutoUpdateCheckerHook").mockReturnValue(async () => {});
  spyOn(registration, "buildAftToolDefinitions").mockReturnValue({});
  spyOn(AftRpcServer.prototype, "start").mockResolvedValue(undefined);

  const plugin = (await import(`../index.js?session-events=${importCounter++}`)).default as Plugin;
  async function instance(directory: string, client: unknown) {
    let nextChannel = 0;
    const routeOpen = mock(async (_target: unknown, _identity: { project_root: string }) => ({
      channel: ++nextChannel,
      epoch: 1,
    }));
    let markSubscribed!: () => void;
    const subscribed = new Promise<void>((resolve) => {
      markSubscribed = resolve;
    });
    const subscribe = mock(() => {
      let close!: () => void;
      const closed = new Promise<void>((resolve) => {
        close = resolve;
      });
      markSubscribed();
      return { unsubscribe: () => close(), closed };
    });
    const pool = new bridge.SubcTransportPool({
      connectionFile: "/unused/session-events.json",
      harness: "opencode",
      connect: async () =>
        ({
          routeOpen,
          subscribe,
          request: async () => ({ structuredContent: { success: true, bg_completions: [] } }),
          closeRouteChannel: async () => {},
          close() {},
        }) as SubcClient,
      onBgEventsNudge() {},
    });
    pools.push(pool);
    // Model an already-connected project transport without prebinding this session.
    spyOn(pool, "getActiveBridgeForRoot").mockImplementation((root) => pool.getBridge(root));
    spyOn(bridge, "createAftTransportPool").mockResolvedValue(pool);
    const instanceHooks = (await plugin({ directory, client } as Parameters<Plugin>[0])) as Hooks;
    hooks.push(instanceHooks);
    return { hooks: instanceHooks, routeOpen, subscribe, subscribed, pool };
  }
  async function nativeInstance(directory: string, session: unknown) {
    const bus = await Effect.runPromise(
      PubSub.unbounded<{ type: string; data: { sessionID: string } }>(),
    );
    const scope = await Effect.runPromise(Scope.make());
    scopes.push(scope);
    const host = {
      location: { directory },
      session: { get: () => Effect.succeed(session) },
      event: { subscribe: () => Stream.fromPubSub(bus) },
    };
    const entry = await instance(directory, host);
    const ctx = { pool: entry.pool, client: host, config: {} } as unknown as PluginContext;
    await Effect.runPromise(
      registerV2SessionLifecycle(host, ctx).pipe(Effect.provideService(Scope.Scope, scope)),
    );
    return {
      ...entry,
      async event(type: string, sessionID: string) {
        await Effect.runPromise(PubSub.publish(bus, { type, data: { sessionID } }));
        await Bun.sleep(20);
      },
    };
  }
  return { first, second, instance, nativeInstance };
}

describe.serial("session events route only to their owning location", () => {
  test("two V2 instances receive one idle but only its owner opens a route and subscription", async () => {
    const { first, second, instance } = await setup();
    const get = () => Effect.succeed({ location: { directory: first } });
    const owner = await instance(first, { location: { directory: first }, session: { get } });
    const other = await instance(second, { location: { directory: second }, session: { get } });
    const idle = { event: { type: "session.idle", properties: { sessionID: "owned" } } };
    // Tracking background work permits an idle completion check before a bridge connects.
    notifications.trackBgTask("owned", "background-task");
    const observed = spyOn(notifications, "observeOpenCodeBgNotificationEvent");

    // Deliver to the non-owner first so shared session state cannot hide an
    // incorrect drain there after the owner's successful drain.
    await other.hooks.event(idle);
    expect(observed).not.toHaveBeenCalled();
    await owner.hooks.event(idle);
    await owner.subscribed;

    expect(observed).toHaveBeenCalledTimes(1);
    expect(other.routeOpen).not.toHaveBeenCalled();
    expect(other.subscribe).not.toHaveBeenCalled();
    expect(owner.routeOpen).toHaveBeenCalledTimes(2);
    expect(owner.subscribe).toHaveBeenCalledTimes(1);
  });

  test("an unresolved V2 session opens no route or subscription in either instance", async () => {
    const { first, second, instance } = await setup();
    const get = () => Effect.succeed(undefined);
    const instances = [];
    for (const directory of [first, second]) {
      instances.push(await instance(directory, { location: { directory }, session: { get } }));
    }
    notifications.trackBgTask("removed-child", "background-task");
    const observed = spyOn(notifications, "observeOpenCodeBgNotificationEvent");
    for (const entry of instances) {
      for (const type of ["session.idle", "session.status"]) {
        await entry.hooks.event({ event: { type, properties: { sessionID: "removed-child" } } });
      }
      expect(entry.routeOpen).not.toHaveBeenCalled();
      expect(entry.subscribe).not.toHaveBeenCalled();
    }
    expect(observed).not.toHaveBeenCalled();
  });

  test("V1 does not open a route or subscription when the idle lookup fails", async () => {
    const { first, instance } = await setup();
    const entry = await instance(first, { session: { get: async () => undefined } });
    notifications.trackBgTask("unknown-v1", "background-task");
    await entry.hooks.event({
      event: { type: "session.idle", properties: { sessionID: "unknown-v1" } },
    });
    expect(entry.routeOpen).not.toHaveBeenCalled();
    expect(entry.subscribe).not.toHaveBeenCalled();
  });

  test("V1 still opens the stored session root after a resume from another cwd", async () => {
    const { first, second, instance } = await setup();
    const entry = await instance(first, {
      session: { get: async () => ({ data: { directory: second } }) },
    });
    notifications.trackBgTask("resumed-v1", "background-task");
    await entry.hooks.event({
      event: { type: "session.idle", properties: { sessionID: "resumed-v1" } },
    });
    await entry.subscribed;
    expect(entry.routeOpen).toHaveBeenCalledTimes(2);
    expect(entry.routeOpen.mock.calls[0][1].project_root).toBe(second);
    expect(entry.subscribe).toHaveBeenCalledTimes(1);
  });

  test("created events are observed only by their V2 owner and never for missing sessions", async () => {
    const { first, second, instance } = await setup();
    const get = ({ sessionID }: { sessionID: string }) =>
      Effect.succeed(sessionID === "new-owned" ? { location: { directory: first } } : undefined);
    const owner = await instance(first, { location: { directory: first }, session: { get } });
    const other = await instance(second, { location: { directory: second }, session: { get } });
    const ownerStart = spyOn(owner.pool, "observeSessionStart");
    const otherStart = spyOn(other.pool, "observeSessionStart");
    for (const sessionID of ["new-owned", "new-removed"]) {
      const created = { event: { type: "session.created", properties: { sessionID } } };
      await other.hooks.event(created);
      await owner.hooks.event(created);
    }
    expect(ownerStart).toHaveBeenCalledTimes(1);
    expect(ownerStart).toHaveBeenCalledWith(first, "new-owned");
    expect(otherStart).not.toHaveBeenCalled();
  });

  test("native V2 deletion cleanup binds only the session's owning Location", async () => {
    const { first, second, nativeInstance } = await setup();
    const session = { location: { directory: first }, parentID: "parent" };
    const owner = await nativeInstance(first, session);
    const other = await nativeInstance(second, session);

    await other.event("session.deleted", "native-owned");
    await owner.event("session.deleted", "native-owned");
    await owner.subscribed;

    expect(other.routeOpen).not.toHaveBeenCalled();
    expect(other.subscribe).not.toHaveBeenCalled();
    expect(owner.routeOpen).toHaveBeenCalledTimes(2);
    expect(owner.routeOpen.mock.calls.every((call) => call[1].project_root === first)).toBe(true);
    expect(owner.subscribe).toHaveBeenCalledTimes(1);
  });

  test("native V2 cleanup opens nothing for a removed child whose lookup has no answer", async () => {
    const { first, second, nativeInstance } = await setup();
    const instances = [
      await nativeInstance(first, undefined),
      await nativeInstance(second, undefined),
    ];
    for (const entry of instances) {
      await entry.event("session.deleted", "native-removed-child");
      expect(entry.routeOpen).not.toHaveBeenCalled();
      expect(entry.subscribe).not.toHaveBeenCalled();
    }
  });
});
