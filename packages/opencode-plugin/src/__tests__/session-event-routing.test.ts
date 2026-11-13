/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import { mkdirSync, mkdtempSync, realpathSync, rmSync } from "node:fs";
import { join, resolve } from "node:path";
import * as bridge from "@cortexkit/aft-bridge";
import { Effect } from "effect";
import { __resetBgNotificationStateForTests } from "../bg-notifications.js";
import * as bootstrap from "../bridge-bootstrap.js";
import * as reload from "../config-live-reload.js";
import * as updater from "../hooks/auto-update-checker/index.js";
import { AftRpcServer } from "../shared/rpc-server.js";
import { _resetSessionDirectoryCacheForTest } from "../shared/session-directory.js";
import * as registration from "../tool-registration.js";

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
let importCounter = 0;

afterEach(async () => {
  await Promise.all(hooks.splice(0).map((instance) => instance.dispose()));
  await Promise.all(pools.splice(0).map((pool) => pool.shutdown()));
  mock.restore();
  __resetBgNotificationStateForTests();
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
    const routeOpen = mock(async (_target: unknown, _identity: { project_root: string }) => ({
      channel: 1,
      epoch: 1,
    }));
    const subscribe = mock(() => ({ unsubscribe() {} }));
    const pool = new bridge.SubcTransportPool({
      connectionFile: "/unused/session-events.json",
      harness: "opencode",
      connect: async () =>
        ({
          routeOpen,
          subscribe,
          request: async () => ({ structuredContent: { success: true, completions: [] } }),
          closeRouteChannel: async () => {},
          close() {},
        }) as SubcClient,
      onBgEventsNudge() {},
    });
    pools.push(pool);
    spyOn(bridge, "createAftTransportPool").mockResolvedValue(pool);
    const instanceHooks = (await plugin({ directory, client } as Parameters<Plugin>[0])) as Hooks;
    hooks.push(instanceHooks);
    return { hooks: instanceHooks, routeOpen, subscribe, pool };
  }
  return { first, second, instance };
}

describe.serial("session events route only to their owning location", () => {
  test("two V2 instances receive one idle but only its owner opens a route and subscription", async () => {
    const { first, second, instance } = await setup();
    const get = () => Effect.succeed({ location: { directory: first } });
    const owner = await instance(first, { location: { directory: first }, session: { get } });
    const other = await instance(second, { location: { directory: second }, session: { get } });
    const idle = { event: { type: "session.idle", properties: { sessionID: "owned" } } };

    // Deliver to the non-owner first so shared session state cannot hide an
    // incorrect drain there after the owner's successful drain.
    await other.hooks.event(idle);
    await owner.hooks.event(idle);

    expect(other.routeOpen).not.toHaveBeenCalled();
    expect(other.subscribe).not.toHaveBeenCalled();
    expect(owner.routeOpen).toHaveBeenCalledTimes(1);
    expect(owner.subscribe).toHaveBeenCalledTimes(1);
  });

  test("an unresolved V2 session opens no route or subscription in either instance", async () => {
    const { first, second, instance } = await setup();
    const get = () => Effect.succeed(undefined);
    const instances = [];
    for (const directory of [first, second]) {
      instances.push(await instance(directory, { location: { directory }, session: { get } }));
    }
    for (const entry of instances) {
      await entry.hooks.event({
        event: { type: "session.idle", properties: { sessionID: "removed-child" } },
      });
      expect(entry.routeOpen).not.toHaveBeenCalled();
      expect(entry.subscribe).not.toHaveBeenCalled();
    }
  });

  test("V1 does not open a route or subscription when the idle lookup fails", async () => {
    const { first, instance } = await setup();
    const entry = await instance(first, { session: { get: async () => undefined } });
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
    await entry.hooks.event({
      event: { type: "session.idle", properties: { sessionID: "resumed-v1" } },
    });
    expect(entry.routeOpen).toHaveBeenCalledTimes(1);
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
});
