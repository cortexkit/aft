import { afterEach, expect, spyOn, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  type BindIdentity,
  type ControlPush,
  type RouteHandle,
  type RouteOpenOptions,
  type RouteScope,
  type RouteTarget,
  SubcClient,
  SubcError,
} from "@cortexkit/subc-client";
import { getActiveLogger, setActiveLogger } from "../active-logger.js";
import type { Logger } from "../logger.js";
import { type SubcClientLike, SubcTransportPool } from "../subc-transport.js";
import { createAftTransportPool } from "../transport-factory.js";
import { TEST_OTHER_ROOT, TEST_PROJECT_ROOT } from "./subc-test-roots.js";

const scope: RouteScope = {
  owner: { kind: "reserved", module_id: "prefrontal-core" },
  ref: "head-scope",
  scopeEpoch: 7,
};

class HostClient implements SubcClientLike {
  readonly opens: Array<{ target: RouteTarget; identity: BindIdentity; opts?: RouteOpenOptions }> =
    [];
  readonly reads: unknown[] = [];
  readonly toolBodies: unknown[] = [];
  readonly toolChannels: number[] = [];
  readonly closedRoutes: number[] = [];
  readonly modules = new Map<number, string>();
  answer: unknown = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
  refusal: string | undefined;
  daemonScopeEpoch = scope.scopeEpoch;
  pendingScopeBinds = 0;
  scopeSyncRefusal = "scope_not_live";
  requestError: SubcError | undefined;
  scopeOpenError: SubcError | undefined;
  scopeOpenGate: Promise<void> | undefined;
  scopedOpenGate: Promise<void> | undefined;
  toolOpenGate: Promise<void> | undefined;
  unscopedOpenGate: Promise<void> | undefined;
  readonly toolOpenErrors: SubcError[] = [];
  toolGate: Promise<void> | undefined;
  subscriptionChannels: number[] = [];
  private readonly subscriptions = new Map<
    number,
    { finish: () => void; fail: (error: Error) => void }
  >();
  private readonly endedChannels = new Set<number>();

  get activeSubscriptionChannels() {
    return [...this.subscriptions.keys()];
  }

  bumpScopeEpoch(epoch: number, refusedBinds: number, code = "scope_not_live") {
    this.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: epoch };
    this.pendingScopeBinds = refusedBinds;
    this.scopeSyncRefusal = code;
  }

  async routeOpen(target: RouteTarget, identity: BindIdentity, opts?: RouteOpenOptions) {
    this.opens.push({ target, identity, opts });
    const channel = this.opens.length;
    if (target.module_id === "prefrontal-core" && this.scopeOpenError) throw this.scopeOpenError;
    if (target.module_id === "prefrontal-core") await this.scopeOpenGate;
    if (target.module_id === "aft") {
      await this.toolOpenGate;
      if (opts?.scope) await this.scopedOpenGate;
      if (!opts?.scope) await this.unscopedOpenGate;
      const error = this.toolOpenErrors.shift();
      if (error) throw error;
    }
    // Core can publish a newer epoch before the daemon has applied its scope snapshot.
    if (opts?.scope && opts.scope.scopeEpoch !== this.daemonScopeEpoch) {
      if (this.pendingScopeBinds > 0) {
        this.pendingScopeBinds--;
        throw new SubcError("scope snapshot not applied", this.scopeSyncRefusal);
      }
      this.daemonScopeEpoch = opts.scope.scopeEpoch;
    }
    if (opts?.scope && this.refusal) throw new SubcError("carrier refused", this.refusal);
    this.modules.set(channel, target.module_id);
    return { channel, epoch: 1 } as RouteHandle;
  }

  async request(route: RouteHandle, body: unknown) {
    if (this.endedChannels.has(route.channel)) throw new SubcError("route gone", "unknown_channel");
    if (this.modules.get(route.channel) === "prefrontal-core") {
      this.reads.push(body);
      // Decode like core's management surface: a body without a string `method`
      // or with params other than {harness, session} is refused, as core does.
      const request = body as { method?: unknown; params?: Record<string, unknown> };
      if (typeof request?.method !== "string") {
        throw new SubcError(
          "route request body has no decodable method: missing field `method`",
          "invalid_request",
        );
      }
      const keys = Object.keys(request.params ?? {}).sort();
      if (keys.join(",") !== "harness,session") {
        throw new SubcError("session.scope_identity params: unknown field", "invalid_request");
      }
      if (this.answer instanceof Error) throw this.answer;
      // Core wraps every management answer as {result: <answer>}.
      return { result: this.answer };
    }
    this.toolBodies.push(body);
    this.toolChannels.push(route.channel);
    await this.toolGate;
    if (this.requestError) {
      const error = this.requestError;
      this.requestError = undefined;
      throw error;
    }
    return { structuredContent: { success: true, text: "ok" } };
  }

  subscribe(route: RouteHandle) {
    this.subscriptionChannels.push(route.channel);
    let finish!: () => void;
    let fail!: (error: Error) => void;
    const closed = new Promise<void>((resolve, reject) => {
      finish = resolve;
      fail = reject;
    }).finally(() => this.subscriptions.delete(route.channel));
    this.subscriptions.set(route.channel, { finish, fail });
    return {
      closed,
      unsubscribe: finish,
    };
  }

  endRoutes(event: ControlPush) {
    if (event.op !== "route.closed") return;
    for (const [channel, module] of this.modules) {
      if (
        Array.isArray(event.body.channels)
          ? !event.body.channels.includes(channel)
          : event.body.module_id !== module
      )
        continue;
      this.endedChannels.add(channel);
      // A module restart sends both a control push and a GOODBYE ending the stream.
      this.subscriptions
        .get(channel)
        ?.fail(
          new SubcError("route closed by subc (GOODBYE)", "route_closed", undefined, "restart"),
        );
    }
  }

  async closeRouteChannel(route: RouteHandle) {
    this.closedRoutes.push(route.channel);
  }

  close() {
    for (const sub of this.subscriptions.values()) sub.fail(new SubcError("client closed"));
  }
}

const pools: SubcTransportPool[] = [];
const originalLogger = getActiveLogger();
const restoreClocks: Array<() => void> = [];
afterEach(async () => {
  await Promise.all(pools.splice(0).map((pool) => pool.shutdown()));
  for (const restore of restoreClocks.splice(0).reverse()) restore();
  setActiveLogger(originalLogger);
});

function fakeClock() {
  let now = Date.now();
  const clock = spyOn(Date, "now").mockImplementation(() => now);
  restoreClocks.push(() => clock.mockRestore());
  return {
    advance: (ms: number) => {
      now += ms;
    },
  };
}

function retryClock() {
  const clock = fakeClock();
  const pending = new Map<ReturnType<typeof setTimeout>, { at: number; fire: () => void }>();
  const active = new Set<ReturnType<typeof setTimeout>>();
  const realSet = globalThis.setTimeout;
  const realClear = globalThis.clearTimeout;
  const retryDelays = new Set([5_000, 10_000, 20_000, 40_000, 80_000, 160_000, 300_000]);
  const set = spyOn(globalThis, "setTimeout").mockImplementation((callback, delay, ...args) => {
    const delayMs = delay ?? 0;
    if (!retryDelays.has(delayMs)) {
      const timer = realSet(() => {
        active.delete(timer);
        callback(...args);
      }, delay);
      active.add(timer);
      return timer;
    }
    const timer = realSet(() => {}, 3_600_000);
    timer.unref();
    active.add(timer);
    pending.set(timer, { at: Date.now() + delayMs, fire: () => callback(...args) });
    return timer;
  });
  const clear = spyOn(globalThis, "clearTimeout").mockImplementation((timer) => {
    pending.delete(timer as ReturnType<typeof setTimeout>);
    active.delete(timer as ReturnType<typeof setTimeout>);
    realClear(timer);
  });
  restoreClocks.push(() => {
    for (const timer of active) realClear(timer);
    clear.mockRestore();
    set.mockRestore();
  });
  return {
    pending,
    active,
    async tick(ms: number) {
      clock.advance(ms);
      for (const [timer, task] of [...pending]) {
        if (task.at > Date.now()) continue;
        pending.delete(timer);
        active.delete(timer);
        realClear(timer);
        task.fire();
      }
      for (let turn = 0; turn < 64; turn++) await Promise.resolve();
    },
  };
}

function scopeLogs(outcomesOnly = false) {
  const lines: string[] = [];
  setActiveLogger({
    log: (line: string) => {
      if (line.startsWith("subc head scope:") && (!outcomesOnly || line.includes(" result=")))
        lines.push(line);
    },
    error() {},
    warn() {},
  } satisfies Logger);
  return lines;
}

function setup(harness = "opencode", background = false, replacement?: HostClient) {
  const client = new HostClient();
  let connected = false;
  let current = client;
  let push!: (event: ControlPush) => void;
  const pool = new SubcTransportPool({
    connectionFile: "/fake/connection.json",
    harness,
    hostScopes: true,
    consumerIdentity: null,
    connect: async (opts) => {
      push = opts.onControlPush!;
      current = connected && replacement ? replacement : client;
      connected = true;
      return current;
    },
    routeRetrySleep: async () => {},
    bgBackoffSleep: async () => {},
    ...(background ? { onBgEventsNudge: () => {} } : {}),
  });
  pools.push(pool);
  return {
    client,
    pool,
    push: (event: ControlPush) => {
      current.endRoutes(event);
      push(event);
    },
  };
}

async function settle() {
  for (let turn = 0; turn < 64; turn++) await Promise.resolve();
}

test("module restart reopens every head route with its cached scope through reload refusals", async () => {
  for (const code of ["module_reloading", "module_warming", "target_unavailable"]) {
    const { client, pool, push } = setup("opencode", true);
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    await bridge.toolCall("head", "read", {});
    await settle();
    const firstOpens = client.opens.length;
    expect(client.subscriptionChannels).toHaveLength(1);
    expect(client.reads).toHaveLength(1);
    client.toolOpenErrors.push(
      new SubcError("AFT restarting", code),
      new SubcError("AFT restarting", code),
    );
    push({ op: "route.closed", body: { module_id: "aft", reason: "restart" } });
    expect((await bridge.toolCall("head", "read", {})).text).toBe("ok");
    await settle();
    const reopened = client.opens.slice(firstOpens);
    expect(reopened.length).toBeGreaterThanOrEqual(4);
    expect(reopened.every((open) => open.target.module_id === "aft")).toBe(true);
    expect(reopened.map((open) => open.opts?.scope)).toEqual(reopened.map(() => scope));
    expect(client.subscriptionChannels).toHaveLength(2);
    expect(client.reads).toHaveLength(1);
    await pool.shutdown();
  }
});

test("head restart refreshes a refused cached scope for bg_events and a third channel", async () => {
  const lines = scopeLogs();
  const { client, pool, push } = setup("opencode", true);
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  await settle();
  expect(client.subscriptionChannels).toHaveLength(1);
  expect(client.reads).toHaveLength(1);

  // A refusal on another carrier must not permanently downgrade the whole session.
  client.refusal = "scope_carrier_denied";
  const other = pool.getBridge(TEST_OTHER_ROOT);
  await other.toolCall("head", "read", {});
  await settle();
  expect(client.opens[client.toolChannels.at(-1)! - 1].opts?.scope).toBeUndefined();
  client.refusal = undefined;

  const firstOpens = client.opens.length;
  client.toolOpenErrors.push(
    new SubcError("AFT restarting", "module_reloading"),
    new SubcError("AFT restarting", "module_reloading"),
  );
  push({ op: "route.closed", body: { module_id: "aft", reason: "restart" } });
  await bridge.toolCall("head", "read", {});
  await other.toolCall("head", "read", {});
  await settle();

  const reopened = client.opens.slice(firstOpens).filter((open) => open.target.module_id === "aft");
  expect(reopened.length).toBeGreaterThanOrEqual(5);
  expect(reopened.map((open) => open.opts?.scope)).toEqual(reopened.map(() => scope));
  expect(client.reads.length).toBeGreaterThan(1);
  expect(
    client.activeSubscriptionChannels
      .map((channel) => client.opens[channel - 1].identity.project_root)
      .sort(),
  ).toEqual([TEST_PROJECT_ROOT, TEST_OTHER_ROOT].sort());
  for (const channel of [...client.toolChannels.slice(-2), ...client.activeSubscriptionChannels]) {
    expect(client.opens[channel - 1].opts?.scope).toEqual(scope);
  }
  const decisions = lines.filter((line) => / bind=(scoped|unscoped) /.test(line));
  const toolOpens = client.opens.filter((open) => open.target.module_id === "aft");
  expect(decisions).toHaveLength(toolOpens.length);
  for (const [index, open] of toolOpens.entries()) {
    expect(decisions[index]).toContain(`bind=${open.opts?.scope ? "scoped" : "unscoped"}`);
    expect(decisions[index]).toContain(`root=${open.identity.project_root}`);
  }
  expect(decisions.every((line) => line.includes("session=head") && line.includes("reason="))).toBe(
    true,
  );
  expect(decisions.some((line) => line.includes("channel=bg_events"))).toBe(true);
  expect(lines.some((line) => line.includes("lookup=skipped reason=cached_scope"))).toBe(true);
  expect(lines.some((line) => line.includes("lookup=started reason=previous_bind_refusal"))).toBe(
    true,
  );
});

test("head bind decisions log every unscoped backoff skip", async () => {
  const lines = scopeLogs();
  const { client, pool } = setup();
  client.answer = new SubcError("core restarting", "module_reloading");
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
  expect(lines.some((line) => line.includes("lookup=skipped reason=backoff"))).toBe(true);
  const binds = lines.filter((line) => line.includes(" bind="));
  expect(binds).toHaveLength(2);
  expect(
    binds.every(
      (line) =>
        line.includes("session=head") && line.includes("reason=lookup_error code=module_reloading"),
    ),
  ).toBe(true);
});

test("a refused head scope keeps background recovery when the fresh lookup is unavailable", async () => {
  const clock = retryClock();
  const lines = scopeLogs();
  const { client, pool, push } = setup("opencode", true);
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  await settle();
  client.refusal = "scope_carrier_denied";
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
  await settle();
  client.refusal = undefined;
  client.scopeOpenError = new SubcError("core restarting", "module_reloading");
  push({ op: "route.closed", body: { module_id: "aft", reason: "restart" } });
  await bridge.toolCall("head", "read", {});
  await settle();
  expect(client.opens[client.toolChannels.at(-1)! - 1].opts?.scope).toBeUndefined();
  expect(clock.pending.size).toBe(1);
  expect(
    lines.some(
      (line) =>
        line.includes("bind=unscoped") &&
        line.includes("reason=lookup_error code=module_reloading"),
    ),
  ).toBe(true);

  client.scopeOpenError = undefined;
  await clock.tick(5_000);
  await bridge.toolCall("head", "read", {});
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
  await settle();
  expect(clock.pending.size).toBe(0);
  expect(client.activeSubscriptionChannels).toHaveLength(2);
  for (const channel of [...client.toolChannels.slice(-2), ...client.activeSubscriptionChannels]) {
    expect(client.opens[channel - 1].opts?.scope).toEqual(scope);
  }
});

test("a cached head scope refusal rechecks core before binding unscoped", async () => {
  const clock = retryClock();
  const { client, pool } = setup();
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  client.refusal = "scope_carrier_denied";
  client.scopeOpenError = new SubcError("core restarting", "module_reloading");
  const other = pool.getBridge(TEST_OTHER_ROOT);
  await other.toolCall("head", "read", {});
  expect(client.opens.filter((open) => open.target.module_id === "prefrontal-core")).toHaveLength(
    2,
  );
  expect(client.opens[client.toolChannels.at(-1)! - 1].opts?.scope).toBeUndefined();
  expect(clock.pending.size).toBe(1);
  client.refusal = undefined;
  client.scopeOpenError = undefined;
  await clock.tick(5_000);
  await other.toolCall("head", "read", {});
  expect(client.opens[client.toolChannels.at(-1)! - 1].opts?.scope).toEqual(scope);
  expect(clock.pending.size).toBe(0);
});

test("client reconnect reopens both head routes with the cached scope", async () => {
  const replacement = new HostClient();
  const { client, pool } = setup("opencode", true, replacement);
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  await settle();
  client.requestError = new SubcError("client closed");
  await expect(bridge.toolCall("head", "read", {})).rejects.toThrow("client closed");
  expect((await bridge.toolCall("head", "read", {})).text).toBe("ok");
  await settle();
  expect(replacement.reads).toHaveLength(0);
  expect(replacement.opens.map((open) => open.opts?.scope)).toEqual([scope, scope]);
  expect(replacement.subscriptionChannels).toHaveLength(1);
});

test("client reconnect waits for a fresh scope when the old lookup is cancelled", async () => {
  const clock = retryClock();
  const replacement = new HostClient();
  const { client, pool } = setup("opencode", true, replacement);
  client.answer = new Error("core starting");
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  await settle();
  let releaseOld!: () => void;
  client.scopeOpenGate = new Promise<void>((resolve) => {
    releaseOld = resolve;
  });
  await clock.tick(5_000);
  let releaseNew!: () => void;
  replacement.scopeOpenGate = new Promise<void>((resolve) => {
    releaseNew = resolve;
  });
  client.requestError = new SubcError("client closed");
  await expect(bridge.toolCall("head", "read", {})).rejects.toThrow("client closed");
  const reconnected = bridge.toolCall("head", "read", {});
  try {
    await settle();
    expect(replacement.opens.filter((open) => open.target.module_id === "aft")).toHaveLength(0);
    expect(
      replacement.opens.filter((open) => open.target.module_id === "prefrontal-core"),
    ).toHaveLength(1);
  } finally {
    releaseOld();
    releaseNew();
    await reconnected;
  }
  await settle();
  expect(replacement.reads).toHaveLength(1);
  const routes = replacement.opens.filter((open) => open.target.module_id === "aft");
  expect(routes.map((open) => open.opts?.scope)).toEqual([scope, scope]);
  expect(replacement.subscriptionChannels).toHaveLength(1);
});

test("background reconnect adopts a scope recovered while its fallback route opens", async () => {
  const clock = retryClock();
  const { client, pool, push } = setup("opencode", true);
  client.answer = new Error("core starting");
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  await settle();
  let release!: () => void;
  client.unscopedOpenGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  push({ op: "route.closed", body: { module_id: "aft", reason: "restart" } });
  await settle();
  const fallbackChannel = client.opens.length;
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  try {
    client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
    await clock.tick(5_000);
    await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
    await bridge.toolCall("head", "read", {});
    expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
  } finally {
    release();
  }
  await settle();
  expect(client.closedRoutes).toContain(fallbackChannel);
  expect(client.subscriptionChannels).not.toContain(fallbackChannel);
  expect(client.subscriptionChannels).toHaveLength(3);
  for (const channel of client.subscriptionChannels.slice(1)) {
    expect(client.opens[channel - 1].opts?.scope).toEqual(scope);
  }
});

test("background scope recovery upgrades a reconnected fallback without a tool route", async () => {
  const clock = retryClock();
  const { client, pool, push } = setup("opencode", true);
  client.answer = new Error("core starting");
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  await settle();
  push({ op: "route.closed", body: { module_id: "aft", reason: "restart" } });
  await settle();
  const fallbackChannel = client.subscriptionChannels.at(-1)!;
  expect(client.opens[fallbackChannel - 1].opts?.scope).toBeUndefined();
  client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
  await clock.tick(5_000);
  expect(client.reads).toHaveLength(2);
  expect(client.closedRoutes).toContain(fallbackChannel);
  expect(client.subscriptionChannels).toHaveLength(3);
  for (const channel of client.subscriptionChannels.slice(2)) {
    expect(client.opens[channel - 1].opts?.scope).toEqual(scope);
  }
});

test("OpenCode and Pi direct carriers bind scoped heads once per host session", async () => {
  for (const harness of ["opencode", "pi"]) {
    const { client, pool } = setup(harness);
    await Promise.all([
      pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {}),
      pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {}),
    ]);
    expect(client.reads).toEqual([
      { method: "session.scope_identity", params: { harness, session: "head" } },
    ]);
    const tools = client.opens.filter((open) => open.target.module_id === "aft");
    expect(tools).toHaveLength(2);
    for (const open of tools) {
      expect(open.opts).toEqual({ consumerIdentity: null, scope });
    }
    expect(client.toolBodies).toEqual([
      { name: "read", arguments: {}, preset: "head" },
      { name: "read", arguments: {}, preset: "head" },
    ]);
    expect(client.closedRoutes).toHaveLength(1);
  }
});

test("both plugin factory harnesses enable host scopes on tool and background routes", async () => {
  const dir = mkdtempSync(join(tmpdir(), "aft-host-scopes-"));
  const connectionFile = join(dir, "connection.json");
  writeFileSync(connectionFile, "{}");
  const connect = spyOn(SubcClient, "connect");
  try {
    for (const harness of ["opencode", "pi"]) {
      const client = new HostClient();
      connect.mockResolvedValue(client as unknown as SubcClient);
      const pool = await createAftTransportPool({
        harness,
        binaryPath: null,
        subcConnectionFile: connectionFile,
        poolOptions: {},
        configOverrides: {},
        subcConsumerIdentity: null,
        onBgEventsNudge: () => {},
      });
      try {
        await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
        for (let turn = 0; turn < 32; turn++) await Promise.resolve();
        expect(client.reads).toHaveLength(1);
        expect(client.subscriptionChannels).toHaveLength(1);
        const tools = client.opens.filter((open) => open.target.module_id === "aft");
        expect(tools).toHaveLength(2);
        expect(tools.map((open) => open.opts?.scope)).toEqual([scope, scope]);
      } finally {
        await pool.shutdown();
      }
    }
  } finally {
    connect.mockRestore();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("absent prefrontal module refuses only the lookup route, not the head bind", async () => {
  const { client, pool } = setup();
  client.scopeOpenError = new SubcError("module absent", "target_unavailable");
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  expect(client.reads).toHaveLength(0);
  expect(client.opens.at(-1)?.target.module_id).toBe("aft");
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
});

test("a stalled scope lookup falls back and closes its late management route", async () => {
  const { client, pool } = setup();
  let release!: () => void;
  client.scopeOpenGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  release();
  for (let turn = 0; turn < 32; turn++) await Promise.resolve();
  expect(client.reads).toHaveLength(0);
  expect(client.closedRoutes).toEqual([1]);
});

test("a timed out head scope lookup retries and binds scoped after backoff", async () => {
  const clock = fakeClock();
  const lines = scopeLogs(true);
  const { client, pool } = setup();
  let release!: () => void;
  client.scopeOpenGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  release();
  for (let turn = 0; turn < 32; turn++) await Promise.resolve();
  expect(client.closedRoutes).toEqual([1]);
  clock.advance(5_000);
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
  expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
  expect(client.reads).toHaveLength(1);
  expect(lines[0]).toMatch(/^subc head scope: session=head result=timeout after=\d+$/);
  expect(lines[1]).toBe("subc head scope: session=head result=scope ref=head-sco epoch=7");
});

test("head scope background retry upgrades a timed out bind without another bind", async () => {
  const clock = retryClock();
  const { client, pool } = setup();
  let release!: () => void;
  client.scopeOpenGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  const oldChannel = client.opens.length;
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  release();
  client.scopeOpenGate = undefined;
  await clock.tick(0);
  expect(clock.pending.size).toBe(1);
  await clock.tick(5_000);
  expect(client.reads).toHaveLength(1);
  expect(client.closedRoutes).toContain(oldChannel);
  expect(client.opens.filter((open) => open.target.module_id === "aft")).toHaveLength(1);
  expect(clock.pending.size).toBe(0);
  await bridge.toolCall("head", "read", {});
  expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
  expect(client.toolChannels).toEqual([oldChannel, client.opens.length]);
});

for (const code of ["scope_not_live", "scope_not_synced", "scope_changed"]) {
  test(`head scope ${code} after an epoch bump recovers without a restart`, async () => {
    const clock = retryClock();
    const { client, pool, push } = setup("pi");
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    await bridge.toolCall("head", "read", {});
    const oldChannel = client.opens.length;
    client.bumpScopeEpoch(8, 3, code);
    push({ op: "route.closed", body: { channels: [oldChannel], reason: "scope_ended" } });
    expect((await bridge.toolCall("head", "read", {})).text).toBe("ok");
    const fallback = client.opens.length;
    expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
    const refusedCounts = [client.pendingScopeBinds];
    for (const delay of [5_000, 10_000]) {
      await clock.tick(delay - 1);
      refusedCounts.push(client.pendingScopeBinds);
      await clock.tick(1);
      expect(client.closedRoutes).not.toContain(fallback);
      refusedCounts.push(client.pendingScopeBinds);
      await bridge.toolCall("head", "read", {});
      expect(client.toolChannels.at(-1)).toBe(fallback);
    }
    await clock.tick(20_000);
    await bridge.toolCall("head", "read", {});
    expect(client.opens.at(-1)?.opts?.scope).toEqual({ ...scope, scopeEpoch: 8 });
    expect(client.closedRoutes).toContain(fallback);
    expect(clock.pending.size).toBe(0);
    expect(client.reads).toHaveLength(2);
    expect(refusedCounts).toEqual([2, 2, 1, 1, 0]);
    expect(client.toolChannels.at(-1)).not.toBe(fallback);
  });
}

test("head scope sync retries back off to five minutes and stop on permanent refusal", async () => {
  const clock = retryClock();
  const { client, pool } = setup("pi");
  client.bumpScopeEpoch(8, 10);
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  const fallback = client.opens.length;
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  for (const delay of [5_000, 10_000, 20_000, 40_000, 80_000, 160_000, 300_000, 300_000]) {
    const before = client.opens.length;
    const refused = client.pendingScopeBinds;
    await clock.tick(delay - 1);
    expect(client.opens).toHaveLength(before);
    await clock.tick(1);
    expect(client.pendingScopeBinds).toBe(refused - 1);
    expect(clock.pending.size).toBe(1);
  }
  client.pendingScopeBinds = 0;
  client.refusal = "scope_not_carrier";
  await clock.tick(300_000);
  expect(clock.pending.size).toBe(0);
  const stoppedAt = client.opens.length;
  client.refusal = undefined;
  await clock.tick(300_000);
  await bridge.toolCall("head", "read", {});
  expect(client.opens).toHaveLength(stoppedAt);
  expect(client.opens.at(-1)?.opts?.scope).toBeDefined();
  expect(client.toolChannels.at(-1)).toBe(fallback);
});

test("head scope sync recovery upgrades the Pi background subscription autonomously", async () => {
  const clock = retryClock();
  const { client, pool } = setup("pi", true);
  client.bumpScopeEpoch(8, 3);
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  await clock.tick(0);
  const fallback = client.toolChannels[0];
  const subscription = client.subscriptionChannels[0];
  expect(client.opens[subscription - 1]?.opts?.scope).toBeUndefined();
  for (const delay of [5_000, 10_000, 20_000]) await clock.tick(delay);
  expect(clock.pending.size).toBe(0);
  expect(client.subscriptionChannels).toHaveLength(2);
  const recovered = client.subscriptionChannels[1];
  expect(client.opens[recovered - 1]?.opts?.scope).toEqual({ ...scope, scopeEpoch: 8 });
  expect(client.closedRoutes).toContain(subscription);
  expect(client.closedRoutes).toContain(fallback);
  await bridge.toolCall("head", "read", {});
  expect(client.opens.at(-1)?.opts?.scope).toEqual({ ...scope, scopeEpoch: 8 });
  expect(client.reads).toHaveLength(1);
});

test("head scope sync recovery preserves a held request until it finishes", async () => {
  const clock = retryClock();
  const lines = scopeLogs();
  const { client, pool } = setup("pi");
  client.bumpScopeEpoch(8, 2);
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  const fallback = client.opens.length;
  let release!: () => void;
  client.toolGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const held = bridge.toolCall("head", "bash", { command: "held", wait: true });
  await clock.tick(5_000);
  expect(client.closedRoutes).not.toContain(fallback);
  await clock.tick(10_000);
  expect(client.closedRoutes).not.toContain(fallback);
  client.toolGate = undefined;
  await bridge.toolCall("head", "read", {});
  expect(client.opens.at(-1)?.opts?.scope).toEqual({ ...scope, scopeEpoch: 8 });
  expect(client.closedRoutes).not.toContain(fallback);
  release();
  expect((await held).text).toBe("ok");
  expect(client.closedRoutes.filter((channel) => channel === fallback)).toHaveLength(1);
  expect(lines).toContain(
    "subc head scope: session=head scoped bind recovered ref=head-sco epoch=8",
  );
  expect(lines.some((line) => line.includes("retry stopped"))).toBe(false);
});

test("head scope route close cancels sync retries and closes a late scoped probe", async () => {
  const clock = retryClock();
  const lines = scopeLogs();
  const { client, pool } = setup("pi");
  client.bumpScopeEpoch(8, 1);
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  let release!: () => void;
  client.toolOpenGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  await clock.tick(5_000);
  const probe = client.opens.length;
  await pool.closeSession(TEST_PROJECT_ROOT, "head");
  client.toolOpenGate = undefined;
  release();
  await clock.tick(0);
  expect(client.closedRoutes).toContain(probe);
  const stoppedAt = client.opens.length;
  await clock.tick(300_000);
  expect(client.opens).toHaveLength(stoppedAt);
  expect(clock.pending.size).toBe(0);
  expect(lines.some((line) => line.includes("scoped bind recovered"))).toBe(false);
  expect(lines).toContain("subc head scope: session=head retry stopped reason=route_closed");
});

test("head scope sync probes are coalesced while another root binds unscoped", async () => {
  const clock = retryClock();
  const { client, pool } = setup("pi");
  client.bumpScopeEpoch(8, 1);
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  let release!: () => void;
  client.scopedOpenGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  await clock.tick(5_000);
  const probe = client.opens.length;
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
  await clock.tick(0);
  expect(client.opens.slice(probe).map((open) => open.opts?.scope)).toEqual([undefined]);
  expect(clock.pending.size).toBe(0);
  client.scopedOpenGate = undefined;
  release();
  await clock.tick(0);
  for (const root of [TEST_PROJECT_ROOT, TEST_OTHER_ROOT]) {
    await pool.getBridge(root).toolCall("head", "read", {});
    expect(client.opens.at(-1)?.opts?.scope).toEqual({ ...scope, scopeEpoch: 8 });
  }
  expect(clock.pending.size).toBe(0);
});

test("head scope background sync bind keeps retrying through provider reload refusals", async () => {
  const clock = retryClock();
  const { client, pool } = setup("pi");
  client.bumpScopeEpoch(8, 1);
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  const fallback = client.opens.length;
  client.toolOpenErrors.push(new SubcError("provider restarting", "module_reloading"));
  await clock.tick(5_000);
  expect(client.closedRoutes).not.toContain(fallback);
  expect(clock.pending.size).toBe(1);
  await clock.tick(10_000);
  await bridge.toolCall("head", "read", {});
  expect(client.opens.at(-1)?.opts?.scope).toEqual({ ...scope, scopeEpoch: 8 });
  expect(clock.pending.size).toBe(0);
});

test("head scope background swap preserves an in-flight held wait request", async () => {
  const clock = retryClock();
  const { client, pool } = setup();
  client.answer = new Error("startup unavailable");
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  const oldChannel = client.opens.length;
  let release!: () => void;
  client.toolGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const held = bridge.toolCall(
    "head",
    "bash",
    { command: "held", wait: true },
    { transportTimeoutMs: 120_000 },
  );
  for (let turn = 0; turn < 32; turn++) await Promise.resolve();
  expect(client.toolChannels).toEqual([oldChannel, oldChannel]);
  client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
  await clock.tick(5_000);
  expect(client.reads).toHaveLength(2);
  expect(client.closedRoutes).not.toContain(oldChannel);
  client.toolGate = undefined;
  await bridge.toolCall("head", "read", {});
  expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
  expect(client.closedRoutes).not.toContain(oldChannel);
  release();
  expect((await held).text).toBe("ok");
  expect(client.closedRoutes.filter((channel) => channel === oldChannel)).toHaveLength(1);
});

test("head scope shutdown cancels a pending background retry timer", async () => {
  const clock = retryClock();
  const { client, pool } = setup();
  client.answer = new Error("startup unavailable");
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  expect(clock.pending.size).toBe(1);
  await pool.shutdown();
  expect(clock.pending.size).toBe(0);
  await clock.tick(300_000);
  expect(client.reads).toHaveLength(1);
});

test("head scope late fallback open adopts a recovered scope before dispatch", async () => {
  const clock = retryClock();
  const { client, pool } = setup();
  client.answer = new Error("startup unavailable");
  let release!: () => void;
  client.toolOpenGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const first = pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  await clock.tick(0);
  const fallbackChannel = client.opens.length;
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
  await clock.tick(5_000);
  expect(client.reads).toHaveLength(2);
  client.toolOpenGate = undefined;
  release();
  expect((await first).text).toBe("ok");
  expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
  expect(client.toolChannels).not.toContain(fallbackChannel);
  expect(client.closedRoutes).toContain(fallbackChannel);
});

test("head scope background retries stop on explicit absence or carrier refusal", async () => {
  const clock = retryClock();
  for (const stop of ["no_scope", "refusal"]) {
    const { client, pool } = setup();
    client.answer = {};
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    await bridge.toolCall("head", "read", {});
    expect(clock.pending.size).toBe(1);
    client.answer =
      stop === "no_scope"
        ? { answer: "no_scope" }
        : { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
    if (stop === "refusal") client.refusal = "scope_carrier_denied";
    await clock.tick(5_000);
    await bridge.toolCall("head", "read", {});
    expect(clock.pending.size).toBe(0);
    await clock.tick(300_000);
    // Revalidate a refused cached scope once before leaving a fallback route in place.
    expect(client.reads).toHaveLength(stop === "refusal" ? 3 : 2);
    expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
    await pool.shutdown();
  }
});

test("head scope shutdown cancels an in-flight background lookup and its deadline", async () => {
  const clock = retryClock();
  const { client, pool } = setup();
  client.answer = new Error("startup unavailable");
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  let release!: () => void;
  client.scopeOpenGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  await clock.tick(5_000);
  expect(clock.active.size).toBe(1);
  const lookupChannel = client.opens.length;
  const stopping = pool.shutdown();
  await clock.tick(0);
  try {
    expect(clock.active.size).toBe(0);
  } finally {
    release();
    await stopping;
  }
  await clock.tick(0);
  expect(client.closedRoutes).toContain(lookupChannel);
  expect(client.reads).toHaveLength(1);
});

test("head scope last route-close push cancels autonomous retries", async () => {
  const clock = retryClock();
  const { client, pool, push } = setup();
  client.answer = new Error("startup unavailable");
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  expect(clock.pending.size).toBe(1);
  push({
    op: "route.closed",
    body: { channels: [client.opens.length], reason: "provider_terminated" },
  });
  expect(clock.pending.size).toBe(0);
  await clock.tick(300_000);
  expect(client.reads).toHaveLength(1);
});

test("head scope background failures retain exponential backoff", async () => {
  const clock = retryClock();
  const { client, pool } = setup();
  client.answer = new Error("startup unavailable");
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  await clock.tick(5_000);
  expect(client.reads).toHaveLength(2);
  await clock.tick(9_999);
  expect(client.reads).toHaveLength(2);
  client.answer = {};
  await clock.tick(1);
  expect(client.reads).toHaveLength(3);
  await clock.tick(19_999);
  expect(client.reads).toHaveLength(3);
  client.answer = { answer: "no_scope" };
  await clock.tick(1);
  expect(client.reads).toHaveLength(4);
  expect(clock.pending.size).toBe(0);
});

test("head scope retry survives one root close and stops when the last route closes", async () => {
  const clock = retryClock();
  const { client, pool } = setup();
  client.answer = new Error("startup unavailable");
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
  expect(clock.pending.size).toBe(1);
  await pool.closeSession(TEST_PROJECT_ROOT, "head");
  expect(clock.pending.size).toBe(1);
  await pool.closeSession(TEST_OTHER_ROOT, "head");
  expect(clock.pending.size).toBe(0);
  await clock.tick(300_000);
  expect(client.reads).toHaveLength(1);
});

test("head scope background swap rebinds the bg_events subscription consistently", async () => {
  const clock = retryClock();
  const { client, pool } = setup("pi", true);
  client.answer = new Error("startup unavailable");
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  await clock.tick(0);
  const oldSubscription = client.subscriptionChannels[0];
  expect(client.subscriptionChannels).toHaveLength(1);
  client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
  await clock.tick(5_000);
  expect(client.subscriptionChannels).toHaveLength(2);
  expect(client.closedRoutes).toContain(oldSubscription);
  expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
});

test("head scope lookup errors are retried rather than cached", async () => {
  for (const source of ["route", "request"]) {
    const clock = fakeClock();
    const { client, pool } = setup();
    if (source === "route")
      client.scopeOpenError = new SubcError("unavailable", "target_unavailable");
    else client.answer = new Error("lookup failed");
    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
    expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
    client.scopeOpenError = undefined;
    client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
    clock.advance(5_000);
    await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
    expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
  }
});

test("explicit no_scope is cached across head binds", async () => {
  const clock = fakeClock();
  const lines = scopeLogs(true);
  const { client, pool } = setup();
  client.answer = { answer: "no_scope" };
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
  clock.advance(300_000);
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
  expect(client.reads).toHaveLength(1);
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  expect(lines).toEqual(["subc head scope: session=head result=no_scope"]);
});

test("malformed head scope replies are retried rather than cached", async () => {
  for (const answer of [
    undefined,
    { answer: "unknown" },
    { owner: scope.owner, ref: scope.ref, scope_epoch: -1 },
  ]) {
    const clock = fakeClock();
    const { client, pool } = setup();
    client.answer = answer;
    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
    expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
    client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
    clock.advance(5_000);
    await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
    expect(client.reads).toHaveLength(2);
    expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
  }
});

test("head scope retries back off exponentially to five minutes without delaying binds", async () => {
  const clock = fakeClock();
  const { client, pool } = setup();
  client.answer = new Error("lookup failed");
  let bind = 0;
  const rebind = () => {
    const root = join(TEST_PROJECT_ROOT, `retry-${bind++}`);
    mkdirSync(root, { recursive: true });
    return pool.getBridge(root).toolCall("head", "read", {});
  };
  await rebind();
  let reads = 1;
  for (const delay of [5_000, 10_000, 20_000, 40_000, 80_000, 160_000, 300_000, 300_000]) {
    clock.advance(delay - 1);
    await rebind();
    expect(client.reads).toHaveLength(reads);
    clock.advance(1);
    await rebind();
    expect(client.reads).toHaveLength(++reads);
  }
  client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
  clock.advance(300_000);
  await rebind();
  expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
});

test("head scope logs once per session outcome change without error messages or full refs", async () => {
  const clock = fakeClock();
  const lines = scopeLogs(true);
  const { client, pool, push } = setup();
  const request = client.request.bind(client);
  let elapsed = 0;
  spyOn(client, "request").mockImplementation(async (route, body) => {
    clock.advance(++elapsed);
    return request(route, body);
  });
  let bind = 0;
  const rebind = () => {
    const root = join(TEST_PROJECT_ROOT, `log-${bind++}`);
    mkdirSync(root, { recursive: true });
    return pool.getBridge(root).toolCall("head", "read", {});
  };
  client.answer = new SubcError("secret message", "target_unavailable");
  await rebind();
  clock.advance(5_000);
  await rebind();
  expect(lines).toHaveLength(1);
  expect(lines[0]).toMatch(
    /^subc head scope: session=head result=error code=target_unavailable after=\d+$/,
  );
  client.answer = new Error("another secret");
  clock.advance(10_000);
  await rebind();
  expect(lines[1]).toMatch(/^subc head scope: session=head result=error code=Error after=\d+$/);
  client.answer = {};
  clock.advance(20_000);
  await rebind();
  clock.advance(40_000);
  await rebind();
  expect(lines).toHaveLength(3);
  expect(lines[2]).toBe("subc head scope: session=head result=malformed");
  client.answer = { owner: scope.owner, ref: "private-ref-123456", scope_epoch: 7 };
  clock.advance(80_000);
  await rebind();
  expect(lines[3]).toBe("subc head scope: session=head result=scope ref=private- epoch=7");
  const channel = client.opens.length;
  push({ op: "route.closed", body: { channels: [channel], reason: "scope_ended" } });
  await rebind();
  expect(lines).toHaveLength(4);
  client.answer = { owner: scope.owner, ref: "private-ref-123456", scope_epoch: 8 };
  push({ op: "route.closed", body: { channels: [client.opens.length], reason: "scope_ended" } });
  await rebind();
  expect(lines[4]).toBe("subc head scope: session=head result=scope ref=private- epoch=8");
  client.answer = { answer: "no_scope" };
  push({ op: "route.closed", body: { channels: [client.opens.length], reason: "scope_ended" } });
  await rebind();
  expect(lines[5]).toBe("subc head scope: session=head result=no_scope");
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("other-head", "read", {});
  expect(lines[6]).toBe("subc head scope: session=other-head result=no_scope");
  expect(lines).toHaveLength(7);
});

test("no_scope, missing prefrontal and lookup errors preserve an unscoped bind", async () => {
  for (const answer of [
    { answer: "no_scope" },
    new SubcError("module absent", "target_unavailable"),
    new Error("lookup failed"),
    { owner: scope.owner, ref: scope.ref, scope_epoch: -1 },
  ]) {
    const { client, pool } = setup();
    client.answer = answer;
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    await bridge.toolCall("head", "read", {});
    await bridge.toolCall("head", "read", {});
    expect(client.reads).toHaveLength(1);
    expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  }
});

test("a scoped open carrier refusal falls back unscoped and rechecks core on the next bind", async () => {
  const warnings: string[] = [];
  setActiveLogger({
    log() {},
    error() {},
    warn: (line: string) => warnings.push(line),
  } satisfies Logger);
  const { client, pool } = setup();
  client.refusal = "scope_carrier_denied";
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
  const tools = client.opens.filter((open) => open.target.module_id === "aft");
  expect(tools.map((open) => open.opts?.scope)).toEqual([scope, undefined, scope, undefined]);
  expect(client.reads).toHaveLength(2);
  expect(warnings).toHaveLength(2);
  expect(warnings.every((line) => line.includes("scope_carrier_denied"))).toBe(true);
});

test("scope end and carrier removal invalidate an idle head cache and re-read its epoch", async () => {
  for (const reason of ["scope_ended", "scope_carrier_removed", "scope_delegation_changed"]) {
    const { client, pool, push } = setup();
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    await bridge.toolCall("head", "read", {});
    const old = client.opens.length;
    client.answer = { owner: scope.owner, ref: "successor", scope_epoch: 8 };
    push({ op: "route.closed", body: { channels: [old], reason } });
    await bridge.toolCall("head", "read", {});
    expect(client.reads).toHaveLength(2);
    expect(client.opens.at(-1)?.opts?.scope).toEqual({ ...scope, ref: "successor", scopeEpoch: 8 });
  }
});

test("stale route epoch re-reads the host scope before retrying", async () => {
  const { client, pool } = setup();
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: 8 };
  client.requestError = new SubcError("epoch changed", "stale_route_epoch");
  await bridge.toolCall("head", "read", {});
  expect(client.reads).toHaveLength(2);
  expect(client.opens.at(-1)?.opts?.scope?.scopeEpoch).toBe(8);
});
