import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { EventEmitter } from "node:events";
import {
  type BindIdentity,
  type RouteHandle,
  type RouteTarget,
  SocketClosedError,
  StaleRouteHandleError,
  SubcCallError,
  SubcError,
} from "@cortexkit/subc-client";
import { getActiveLogger, setActiveLogger } from "../active-logger.js";
import type { Logger, LogMeta } from "../logger.js";
import { type SubcClientLike, SubcTransportPool } from "../subc-transport.js";
import { TEST_OTHER_ROOT, TEST_PROJECT_ROOT } from "./subc-test-roots.js";

/** A controllable held-open subscription handle. */
class FakeSubscription {
  unsubscribed = 0;
  private resolveClosed!: () => void;
  private rejectClosed!: (err: Error) => void;
  readonly closed: Promise<void>;

  constructor(
    readonly channel: number,
    readonly onEvent: (event: Uint8Array) => void,
    private readonly unsubscribeGate: Promise<void> | null = null,
  ) {
    this.closed = new Promise<void>((resolve, reject) => {
      this.resolveClosed = resolve;
      this.rejectClosed = reject;
    });
    // Swallow the rejection if nobody is awaiting yet (the loop attaches its own).
    this.closed.catch(() => undefined);
  }

  /** Fire a bg_events nudge to the subscriber. */
  emit(): void {
    this.onEvent(new TextEncoder().encode(JSON.stringify({ op: "bg_events" })));
  }

  /** Simulate a socket drop / route GOODBYE (non-transient) — resubscribe, keep client. */
  drop(): void {
    this.rejectClosed(new Error("subscription dropped"));
  }

  /** Simulate a dead-CONNECTION drop (transient) — resubscribe AND drop the client. */
  dropTransient(): void {
    this.rejectClosed(new SocketClosedError("subc socket closed"));
  }

  /** Simulate a provider StreamEnd — the loop should resubscribe unless locally stopped. */
  end(): void {
    this.resolveClosed();
  }

  unsubscribe(): void {
    this.unsubscribed += 1;
    if (this.unsubscribeGate) {
      void this.unsubscribeGate.then(() => this.resolveClosed());
      return;
    }
    this.resolveClosed();
  }
}

function fakeRouteHandle(channel: number): RouteHandle {
  return { channel, epoch: channel } as RouteHandle;
}

/** Records every routeOpen/request/subscribe so a test can assert caching + bodies. */
class FakeClient implements SubcClientLike {
  routeOpens: BindIdentity[] = [];
  routeConsumerIdentities: Array<{ module_id: string; launch_nonce: string } | null | undefined> =
    [];
  requests: {
    route: RouteHandle;
    channel: number;
    body: unknown;
    options?: { timeoutMs?: number };
  }[] = [];
  subscriptions: FakeSubscription[] = [];
  closedRoutes: number[] = [];
  closed = 0;
  droppedIngressFrames = 0;
  nextChannel = 1;
  subscriptionUnsubscribeGate: Promise<void> | null = null;
  /** When set, routeOpen awaits this gate before resolving (race control). */
  routeOpenGate: Promise<void> | null = null;
  /** When set, the NEXT routeOpen rejects with this error then clears it. */
  routeOpenError: Error | null = null;
  /** Returns an error while the mock daemon is refusing every route reopen. */
  routeOpenFailure: (() => Error | null) | null = null;
  /** Simulates subc-client's one ingress dispatcher shared by all subscriptions. */
  private readonly ingress = new EventEmitter();

  constructor(private readonly onRequest: (channel: number, body: unknown) => Promise<unknown>) {}

  get ingressListenerCount(): number {
    return this.ingress.listenerCount("frame");
  }

  async routeOpen(
    _target: RouteTarget,
    identity: BindIdentity,
    opts?: { consumerIdentity?: { module_id: string; launch_nonce: string } | null },
  ): Promise<RouteHandle> {
    this.routeOpens.push(identity);
    this.routeConsumerIdentities.push(opts?.consumerIdentity);
    const daemonError = this.routeOpenFailure?.();
    if (daemonError) throw daemonError;
    if (this.routeOpenError) {
      const err = this.routeOpenError;
      this.routeOpenError = null;
      throw err;
    }
    if (this.routeOpenGate) await this.routeOpenGate;
    return fakeRouteHandle(this.nextChannel++);
  }

  async request(
    route: RouteHandle,
    body: unknown,
    options?: { timeoutMs?: number },
  ): Promise<unknown> {
    this.requests.push({ route, channel: route.channel, body, options });
    return this.onRequest(route.channel, body);
  }

  subscribe(
    route: RouteHandle,
    _body: unknown,
    onEvent: (event: Uint8Array) => void,
  ): FakeSubscription {
    const sub = new FakeSubscription(route.channel, onEvent, this.subscriptionUnsubscribeGate);
    const ingressListener = (event: Uint8Array): void => onEvent(event);
    this.ingress.on("frame", ingressListener);
    // The callback belongs to this subscription lifecycle. Settling `closed` on
    // every StreamEnd/GOODBYE releases it before a reconnect can subscribe again.
    void sub.closed.then(
      () => this.ingress.off("frame", ingressListener),
      () => this.ingress.off("frame", ingressListener),
    );
    this.subscriptions.push(sub);
    return sub;
  }

  async closeRouteChannel(route: RouteHandle): Promise<void> {
    this.closedRoutes.push(route.channel);
  }

  close(): void {
    this.closed += 1;
  }
}

/** Yield to the microtask/timer queue so the bg loop can advance. */
async function tick(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

async function settleMicrotasks(turns = 32): Promise<void> {
  for (let turn = 0; turn < turns; turn += 1) await Promise.resolve();
}

/** Deterministic timer seam for asserting parked reconnect work without real waits. */
class FakeClock {
  private now = 0;
  private nextTimerId = 0;
  private readonly timers = new Map<number, { dueAt: number; resolve: () => void }>();
  readonly scheduledDelays: number[] = [];

  sleep = (ms: number): Promise<void> => {
    this.scheduledDelays.push(ms);
    return new Promise((resolve) => {
      this.timers.set(this.nextTimerId++, { dueAt: this.now + ms, resolve });
    });
  };

  get scheduledWorkCount(): number {
    return this.timers.size;
  }

  async advance(ms: number): Promise<void> {
    this.now += ms;
    const ready = [...this.timers.entries()]
      .filter(([, timer]) => timer.dueAt <= this.now)
      .sort(([, left], [, right]) => left.dueAt - right.dueAt);
    for (const [id, timer] of ready) {
      this.timers.delete(id);
      timer.resolve();
    }
    await settleMicrotasks();
  }
}

function poolWith(
  client: FakeClient,
  harness = "opencode",
): { pool: SubcTransportPool; connects: number } {
  const state = { connects: 0 };
  const pool = new SubcTransportPool({
    connectionFile: "/tmp/fake-subc-connection.json",
    harness,
    connect: async () => {
      state.connects += 1;
      return client;
    },
  });
  return {
    pool,
    get connects() {
      return state.connects;
    },
  } as { pool: SubcTransportPool; connects: number };
}

// The Rust module wraps the flat response under structuredContent (S1 envelope).
function envelope(flat: Record<string, unknown>): Record<string, unknown> {
  return {
    content: [{ type: "text", text: flat.text }],
    isError: flat.success === false,
    structuredContent: flat,
  };
}

// This fixture preserves the field order from a real read response: the Rust wire
// serializer emits `text` as the final structured field.
const CAPTURED_FIRST_PARTY_READ_ENVELOPE: Record<string, unknown> = {
  content: [
    {
      type: "text",
      text: "export function readFixture(): string { return 'captured'; }\n",
    },
  ],
  isError: false,
  structuredContent: {
    id: "read-fixture-1",
    success: true,
    bg_completions: [{ task_id: "bash-fixture-1" }],
    text: "export function readFixture(): string { return 'captured'; }\n",
  },
};

function withoutStructuredText(envelopeResponse: Record<string, unknown>): Record<string, unknown> {
  const structuredContent = {
    ...(envelopeResponse.structuredContent as Record<string, unknown>),
  };
  delete structuredContent.text;
  return { ...envelopeResponse, structuredContent };
}

describe("SubcTransport.toolCall", () => {
  test("sends {name, arguments} and re-lifts structuredContent to the flat result", async () => {
    const client = new FakeClient(async () =>
      envelope({
        id: "req-1",
        success: true,
        text: "rendered output",
        bg_completions: [{ task_id: "bash-1" }],
      }),
    );
    const { pool } = poolWith(client);

    const result = await pool
      .getBridge(TEST_PROJECT_ROOT)
      .toolCall("sess-1", "read", { filePath: "a.ts" });

    // Body is the tool-route shape, NOT {method, params}.
    expect(client.requests[0]?.body).toEqual({
      name: "read",
      arguments: { filePath: "a.ts" },
    });
    // The adapter promotes supported fields to the top-level result, preserving
    // background completions while excluding the retired status_bar field.
    expect(result.success).toBe(true);
    expect(result.text).toBe("rendered output");
    expect(result).not.toHaveProperty("status_bar");
    expect(result.bg_completions).toEqual([{ task_id: "bash-1" }]);
  });

  test("carries plugin edit registration outside agent arguments", async () => {
    const client = new FakeClient(async () => envelope({ success: true, text: "ok" }));
    const { pool } = poolWith(client);
    pool.setConfigureOverride("edit_slot_survives", true);

    await pool
      .getBridge(TEST_PROJECT_ROOT)
      .toolCall("sess", "edit", { patch: "[a.ts#TAG]\nPUT 1:\n+x" });

    expect(client.requests[0]?.body).toEqual({
      name: "edit",
      arguments: { patch: "[a.ts#TAG]\nPUT 1:\n+x" },
      edit_slot_survives: true,
    });
  });

  test("carries plugin edit registration on native bash dispatch", async () => {
    const client = new FakeClient(async () => envelope({ success: true, text: "ok" }));
    const { pool } = poolWith(client);
    pool.setConfigureOverride("edit_slot_survives", true);

    await pool.getBridge(TEST_PROJECT_ROOT).send("bash", {
      session_id: "sess",
      command: "cat build.rs",
    });

    expect(client.requests[0]?.body).toEqual({
      name: "bash",
      arguments: { session_id: "sess", command: "cat build.rs" },
      edit_slot_survives: true,
    });
  });

  test("forwards an explicit direct consumer identity override to route.open", async () => {
    const client = new FakeClient(async () => envelope({ success: true, text: "ok" }));
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake-subc-connection.json",
      harness: "opencode",
      consumerIdentity: null,
      connect: async () => client,
    });

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess", "read", { filePath: "a.ts" });

    expect(client.routeConsumerIdentities).toEqual([null]);
  });

  test("preview:true is placed at the top level of the request body", async () => {
    const client = new FakeClient(async () =>
      envelope({ id: "r", success: true, text: "preview" }),
    );
    const { pool } = poolWith(client);

    await pool
      .getBridge(TEST_PROJECT_ROOT)
      .toolCall("s", "edit", { oldString: "a" }, { preview: true });

    expect(client.requests[0]?.body).toEqual({
      name: "edit",
      arguments: { oldString: "a" },
      preview: true,
    });
  });

  test("transportTimeoutMs (wait-aware bash budget) reaches the wire request deadline", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "ok" }));
    const { pool } = poolWith(client);
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    // Orchestrated bash passes its wait-aware budget as transportTimeoutMs;
    // it must win over timeoutMs and reach client.request, otherwise long
    // commands die at the client's default unary deadline mid-execution.
    await t.toolCall(
      "s",
      "bash",
      { command: "sleep 100" },
      {
        transportTimeoutMs: 905_000,
        timeoutMs: 60_000,
      },
    );
    expect(client.requests[0]?.options?.timeoutMs).toBe(905_000);

    // Plain per-command override still applies when no orchestrated budget.
    await t.toolCall("s", "grep", { query: "x" }, { timeoutMs: 60_000 });
    expect(client.requests[1]?.options?.timeoutMs).toBe(60_000);
  });

  test("caches the route per (root, harness, session) and reuses it", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    await t.toolCall("sess-A", "read", {});
    await t.toolCall("sess-A", "grep", {}); // same identity -> same channel, no new routeOpen
    await t.toolCall("sess-B", "read", {}); // different session -> new route

    expect(client.routeOpens.length).toBe(2);
    expect(client.routeOpens[0]?.session).toBe("sess-A");
    expect(client.routeOpens[1]?.session).toBe("sess-B");
    // The exact opaque handle is reused for one session; another session gets
    // a different identity even independently of its numeric fields.
    expect(client.requests[0]?.route).toBe(client.requests[1]?.route);
    expect(client.requests[2]?.route).not.toBe(client.requests[0]?.route);
  });

  test("session-less call falls back to the __default__ session", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall(undefined, "read", {});

    expect(client.routeOpens[0]?.session).toBe("__default__");
  });

  test("a tool-level success:false reply is returned, not thrown", async () => {
    const client = new FakeClient(async () =>
      envelope({ id: "r", success: false, code: "path_not_found", text: "no such file" }),
    );
    const { pool } = poolWith(client);

    const result = await pool.getBridge(TEST_PROJECT_ROOT).toolCall("s", "read", {});
    expect(result.success).toBe(false);
    expect(result.code).toBe("path_not_found");
  });
});

describe("SubcTransport Rd reconnect", () => {
  // The raw request() path rejects with REAL error types — base SubcError
  // (timeout / route GOODBYE / daemon Error frame) or a socket error (closed /
  // reset / pre-send write failure) — and NEVER a managed SubcCallError. These
  // tests use those real types so the `isConsumerReconnectTransient` classifier
  // is exercised exactly as it will be in production (a prior version faked
  // SubcCallError, which the classifier treats as transient and so masked the
  // wrong-instanceof bug).

  test("a dead-socket error (transient) drops the channel AND client; next call reconnects", async () => {
    let calls = 0;
    let madeClients = 0;
    const onRequest = async (): Promise<unknown> => {
      calls += 1;
      if (calls === 1) throw new SocketClosedError("subc socket closed");
      return envelope({ id: "r", success: true, text: "recovered" });
    };
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        return new FakeClient(onRequest);
      },
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    // First call surfaces the transport error (Rd never auto-retries).
    await expect(t.toolCall("s", "read", {})).rejects.toBeInstanceOf(SocketClosedError);

    // Second call reconnects (a NEW client from the factory) and recovers.
    const result = await t.toolCall("s", "read", {});
    expect(result.text).toBe("recovered");
    expect(madeClients).toBe(2); // the dead client was dropped, a fresh one connected
  });

  test.each([
    ["unknown_channel", () => new SubcError("unknown channel 1", "unknown_channel")],
    ["stale_route_handle", () => new StaleRouteHandleError(fakeRouteHandle(1))],
  ])("%s reopens the route and resends once", async (_name, routeAbsentError) => {
    let calls = 0;
    const client = new FakeClient(async () => {
      calls += 1;
      if (calls === 1) throw routeAbsentError();
      return envelope({ id: "r", success: true, text: "resent after reopen" });
    });
    const { pool } = poolWith(client);
    const transport = pool.getBridge(TEST_PROJECT_ROOT);

    const result = await transport.toolCall("s", "write", { filePath: "a.txt", content: "ok" });

    expect(result.success).toBe(true);
    expect(result.text).toBe("resent after reopen");
    expect(client.routeOpens.length).toBe(2);
    expect(client.requests.length).toBe(2);
    expect(client.requests[0]?.channel).toBe(1);
    expect(client.requests[1]?.channel).toBe(2);
    expect(client.requests[1]?.body).toEqual(client.requests[0]?.body);
    expect(client.closedRoutes).toEqual([1]);
  });

  test("unknown_channel retries share a 100ms restart-burst delay", async () => {
    let requestAttempts = 0;
    const client = new FakeClient(async () => {
      requestAttempts += 1;
      if (requestAttempts <= 2) throw new SubcError("unknown channel", "unknown_channel");
      return envelope({ id: "r", success: true, text: "recovered" });
    });
    const clock = new FakeClock();
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => client,
      routeRetrySleep: clock.sleep,
    });
    const transport = pool.getBridge(TEST_PROJECT_ROOT);

    const recovered = Promise.all([
      transport.toolCall("restart-1", "read", {}),
      transport.toolCall("restart-2", "read", {}),
    ]);
    await settleMicrotasks();

    // Both route-closing replies arrived together, but they share one floor timer
    // before either request is resent onto the restarted daemon.
    expect(client.routeOpens).toHaveLength(2);
    expect(clock.scheduledWorkCount).toBe(1);
    await clock.advance(99);
    expect(client.routeOpens).toHaveLength(2);
    await clock.advance(1);

    await expect(recovered).resolves.toHaveLength(2);
    expect(client.routeOpens).toHaveLength(4);
    expect(clock.scheduledDelays).toEqual([100]);
  });

  test("reload-window route.open refusals share the restart-burst floor and stay invisible", async () => {
    const reloadErrors: Error[] = [
      new SubcError("module_id 'aft' is reloading", "module_reloading"),
      new SubcError("module_id 'aft' is reloading", "module_reloading"),
      new SubcError("module_id 'aft' is reloading", "module_reloading"),
      Object.assign(new SubcError("module supervised but not available", "module_warming"), {
        state: "running",
        enabled: true,
        live: false,
      }),
      new SubcError("connection closed during route.bind relay", "target_unavailable"),
    ];
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "live" }));
    client.routeOpenFailure = () => reloadErrors.shift() ?? null;
    const clock = new FakeClock();
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => client,
      routeRetrySleep: clock.sleep,
    });
    const transport = pool.getBridge(TEST_PROJECT_ROOT);

    const calls = Promise.all([
      transport.toolCall("reload-1", "read", {}),
      transport.toolCall("reload-2", "read", {}),
    ]);
    await settleMicrotasks();
    expect(clock.scheduledWorkCount).toBe(1);
    await clock.advance(100);
    expect(clock.scheduledWorkCount).toBe(1);
    await clock.advance(200);
    expect(clock.scheduledWorkCount).toBe(1);
    await clock.advance(400);

    await expect(calls).resolves.toHaveLength(2);
    expect(client.requests).toHaveLength(2);
    expect(clock.scheduledDelays).toEqual([100, 200, 400]);
  });

  test("reload-window exhaustion surfaces the final route.open error with a locked suffix", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "never" }));
    client.routeOpenFailure = () =>
      new SubcError("module_id 'aft' is reloading", "module_reloading");
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => client,
      routeRetrySleep: async () => undefined,
    });

    let surfaced: unknown;
    try {
      await pool.getBridge(TEST_PROJECT_ROOT).toolCall("reload-exhausted", "read", {});
    } catch (error) {
      surfaced = error;
    }
    expect((surfaced as Error).message).toBe(
      "module_id 'aft' is reloading The AFT daemon module did not return within the 15s reload window.",
    );
  });

  test("a dying-route GOODBYE surfaces while a fresh reload-window dispatch is absorbed", async () => {
    let releaseDying!: () => void;
    const dyingGate = new Promise<void>((resolve) => {
      releaseDying = resolve;
    });
    let markDyingRequestStarted!: () => void;
    const dyingRequestStarted = new Promise<void>((resolve) => {
      markDyingRequestStarted = resolve;
    });
    const goodbye = new SubcError("route closed by subc (GOODBYE)", "route_closed");
    let requestCalls = 0;
    const client = new FakeClient(async () => {
      requestCalls += 1;
      if (requestCalls === 1) {
        markDyingRequestStarted();
        await dyingGate;
        throw goodbye;
      }
      return envelope({ id: "fresh", success: true, text: "live" });
    });
    const reloadErrors: Error[] = [
      new SubcError("module_id 'aft' is reloading", "module_reloading"),
      Object.assign(new SubcError("module supervised but not available", "target_unavailable"), {
        state: "running",
        enabled: true,
        live: false,
      }),
    ];
    const clock = new FakeClock();
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => client,
      routeRetrySleep: clock.sleep,
    });
    const transport = pool.getBridge(TEST_PROJECT_ROOT);

    const dying = transport.toolCall("dying-route", "write", { filePath: "a", content: "old" });
    await dyingRequestStarted;
    client.routeOpenFailure = () => reloadErrors.shift() ?? null;
    const fresh = transport.toolCall("fresh-route", "write", { filePath: "a", content: "new" });
    await settleMicrotasks();
    expect(clock.scheduledWorkCount).toBe(1);

    releaseDying();
    await expect(dying).rejects.toBe(goodbye);
    expect(client.requests).toHaveLength(1);

    await clock.advance(100);
    await clock.advance(200);
    await expect(fresh).resolves.toMatchObject({ text: "live" });
    expect(client.requests).toHaveLength(2);
  });

  test("unknown-channel resends and reload-window opens share one restart-burst timer", async () => {
    let requestAttempts = 0;
    const client = new FakeClient(async () => {
      requestAttempts += 1;
      if (requestAttempts <= 2) throw new SubcError("unknown channel", "unknown_channel");
      return envelope({ id: "r", success: true, text: "recovered" });
    });
    const reloadErrors: Error[] = [
      new SubcError("module_id 'aft' is reloading", "module_reloading"),
      new SubcError("module_id 'aft' is reloading", "module_reloading"),
    ];
    const clock = new FakeClock();
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => client,
      routeRetrySleep: clock.sleep,
    });
    const transport = pool.getBridge(TEST_PROJECT_ROOT);
    const calls = Promise.all([
      transport.toolCall("interleaved-1", "read", {}),
      transport.toolCall("interleaved-2", "read", {}),
    ]);
    await settleMicrotasks();
    client.routeOpenFailure = () => reloadErrors.shift() ?? null;

    expect(clock.scheduledWorkCount).toBe(1);
    await clock.advance(100);
    expect(clock.scheduledWorkCount).toBe(1);
    await clock.advance(200);

    await expect(calls).resolves.toHaveLength(2);
    expect(client.requests).toHaveLength(4);
    expect(clock.scheduledDelays).toEqual([100, 200]);
  });

  test("outcome-unknown request failures still surface without an in-place retry", async () => {
    const outcomeUnknown = new SubcCallError(
      "outcome_unknown",
      "connection dropped after the request was queued",
    );
    const client = new FakeClient(async () => {
      throw outcomeUnknown;
    });
    const { pool } = poolWith(client);
    const transport = pool.getBridge(TEST_PROJECT_ROOT);

    await expect(
      transport.toolCall("s", "write", { filePath: "a.txt", content: "ok" }),
    ).rejects.toBe(outcomeUnknown);
    expect(client.routeOpens.length).toBe(1);
    expect(client.requests.length).toBe(1);
    expect(client.closed).toBe(1);
  });

  test("a not-queued write failure (transient, not_sent-equivalent) drops the client", async () => {
    // SubcWriteNotQueuedError is the raw-path analog of `not_sent`: bytes never
    // left the local socket. isConsumerReconnectTransient classifies it transient.
    let calls = 0;
    let madeClients = 0;
    const notQueued = Object.assign(new Error("write not queued"), { code: "EPIPE" });
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        return new FakeClient(async () => {
          calls += 1;
          if (calls === 1) throw notQueued; // EPIPE -> transient
          return envelope({ id: "r", success: true, text: "ok" });
        });
      },
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);
    await expect(t.toolCall("s", "read", {})).rejects.toBe(notQueued);
    await t.toolCall("s", "read", {});
    expect(madeClients).toBe(2);
  });

  test("a plain timeout (non-transient SubcError) KEEPS the client, drops only the route", async () => {
    // Q1: a lost/late response does NOT prove the connection is dead. Keep the
    // client (no reconnect); the route is re-opened on the next call. Mutation-
    // safe: the error is surfaced, never auto-retried.
    let calls = 0;
    let madeClients = 0;
    const client = new FakeClient(async () => {
      calls += 1;
      if (calls === 1) throw new SubcError("request on channel 1 timed out after 30000ms");
      return envelope({ id: "r", success: true, text: "second" });
    });
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        return client;
      },
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    expect(client.closed).toBe(0); // client kept alive
    // The route was dropped, so the next call re-opens it on the SAME client.
    const result = await t.toolCall("s", "edit", {});
    expect(result.text).toBe("second");
    expect(madeClients).toBe(1); // never reconnected
    expect(client.routeOpens.length).toBe(2); // route re-opened
    expect(calls).toBe(2); // exactly two underlying requests — no auto-retry
  });

  test("a route GOODBYE carrying kind=outcome_unknown KEEPS the client until the classifier reads it", async () => {
    // A module restart sends GOODBYE to every route on the connection, so a
    // mid-request call rejects with a SubcError (the raw request() path never
    // produces a managed SubcCallError). isConsumerReconnectTransient reaches
    // `err instanceof SubcCallError` first, so today that error is
    // non-transient: route dropped, client KEPT, next call re-opens on the
    // same connection.
    //
    // The fixture carries `kind` even though today's SubcError has only
    // `code`. That is the point, and it is what makes this more than a
    // restatement of the timeout test above: cortexkit/subconscious#6 proposes
    // moving `kind` onto SubcError, and a route GOODBYE is outcome-unknown by
    // construction (the daemon sends it after the drain wait regardless of
    // execution status — see the disposition in error-contract.ts). So this is
    // the error shape the SDK will hand us post-#6. The behaviour flip does not
    // come from the error changing; it comes from the classifier starting to
    // READ this field. Modelling the field now is what lets that flip land as a
    // red test instead of as a live reconnect-policy change on an SDK bump.
    //
    // Characterization, not endorsement — the flip is probably correct. When it
    // lands, flip the assertions and keep the test.
    let calls = 0;
    let madeClients = 0;
    const goodbye = Object.assign(new SubcError("route closed by subc (GOODBYE)"), {
      kind: "outcome_unknown",
    });
    const client = new FakeClient(async () => {
      calls += 1;
      if (calls === 1) throw goodbye;
      return envelope({ id: "r", success: true, text: "after-restart" });
    });
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        return client;
      },
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    expect(client.closed).toBe(0); // client kept — reading `kind` would make this 1
    const result = await t.toolCall("s", "edit", {});
    expect(result.text).toBe("after-restart");
    expect(madeClients).toBe(1); // never reconnected — reading `kind` would make this 2
    expect(client.routeOpens.length).toBe(2); // route re-opened on the same client
    expect(calls).toBe(2); // no in-place retry: a GOODBYE mid-call is outcome-unknown
  });
});

describe("SubcTransport reply envelope (B-#7)", () => {
  test("synthesizes missing structuredContent.text from a single outer text block", async () => {
    const duplicatedClient = new FakeClient(async () => CAPTURED_FIRST_PARTY_READ_ENVELOPE);
    const { pool: duplicatedPool } = poolWith(duplicatedClient);
    const duplicated = await duplicatedPool
      .getBridge(TEST_PROJECT_ROOT)
      .toolCall("sess-1", "read", { filePath: "fixture.ts" });

    const synthesizedClient = new FakeClient(async () =>
      withoutStructuredText(CAPTURED_FIRST_PARTY_READ_ENVELOPE),
    );
    const { pool: synthesizedPool } = poolWith(synthesizedClient);
    const synthesized = await synthesizedPool
      .getBridge(TEST_PROJECT_ROOT)
      .toolCall("sess-1", "read", { filePath: "fixture.ts" });

    const capturedBytes = new TextEncoder().encode(
      JSON.stringify(CAPTURED_FIRST_PARTY_READ_ENVELOPE.structuredContent),
    );
    expect(new TextEncoder().encode(JSON.stringify(duplicated))).toEqual(capturedBytes);
    expect(new TextEncoder().encode(JSON.stringify(synthesized))).toEqual(capturedBytes);
  });

  test("present structuredContent.text wins without inspecting outer content", async () => {
    const response = {
      ...CAPTURED_FIRST_PARTY_READ_ENVELOPE,
      content: [{ type: "image", data: "not inspected" }],
    };
    const client = new FakeClient(async () => response);
    const { pool } = poolWith(client);

    const result = await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {});

    expect(new TextEncoder().encode(JSON.stringify(result))).toEqual(
      new TextEncoder().encode(
        JSON.stringify(CAPTURED_FIRST_PARTY_READ_ENVELOPE.structuredContent),
      ),
    );
  });

  test.each([
    ["zero blocks", []],
    [
      "multiple blocks",
      [
        { type: "text", text: "first" },
        { type: "text", text: "second" },
      ],
    ],
    ["non-text block", [{ type: "image", data: "not text" }]],
  ])("missing structuredContent.text with %s stays fail-closed", async (_name, content) => {
    const client = new FakeClient(async () => ({
      content,
      isError: false,
      structuredContent: { id: "read-1", success: true },
    }));
    const { pool } = poolWith(client);

    await expect(pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {})).rejects.toThrow(
      "subc tool reply structuredContent lacks a boolean `success` / string `text` (protocol violation)",
    );
  });

  test("a reply missing the structuredContent envelope throws (protocol violation)", async () => {
    // No structuredContent → must NOT be coerced to a silent {success:false}.
    const client = new FakeClient(async () => ({ content: [], isError: false }));
    const { pool } = poolWith(client);
    await expect(pool.getBridge(TEST_PROJECT_ROOT).toolCall("s", "read", {})).rejects.toThrow(
      /structuredContent envelope/,
    );
  });

  test("a structuredContent without boolean success throws (cannot read as success)", async () => {
    const client = new FakeClient(async () => ({
      content: [],
      isError: false,
      structuredContent: { text: "x" }, // success is undefined
    }));
    const { pool } = poolWith(client);
    await expect(pool.getBridge(TEST_PROJECT_ROOT).toolCall("s", "read", {})).rejects.toThrow(
      /boolean `success`/,
    );
  });
});

describe("SubcTransportPool route lifecycle (B-#3/#4/#5)", () => {
  test("R4 #2: a stale non-transient failure does not delete a SUCCESSOR route on the same client", async () => {
    // R1 opens route E (channel 1) and is held. closeSession deletes E. R2 opens a
    // SUCCESSOR E2 (channel 2) on the SAME still-current client. R1 then fails LATE
    // with a non-transient error (client kept). Its catch must delete only ITS OWN
    // entry E — not the successor E2 (the client guard alone passes here; the entry
    // token is what protects E2).
    let releaseR1!: () => void;
    const gate = new Promise<void>((r) => {
      releaseR1 = r;
    });
    let calls = 0;
    const client = new FakeClient(async () => {
      calls += 1;
      if (calls === 1) {
        await gate;
        throw new SubcError("R1 late timeout"); // non-transient → client KEPT
      }
      return envelope({ id: "r", success: true, text: "ok" });
    });
    const { pool } = poolWith(client); // no bg sub → routeOpens count is just tool routes
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    const r1 = t.toolCall("sess-1", "read", {}).catch((e) => e); // opens E (ch 1), held
    await tick();
    await pool.closeSession(TEST_PROJECT_ROOT, "sess-1"); // deletes E, closes ch 1
    const r2 = await t.toolCall("sess-1", "read", {}); // opens successor E2 (ch 2)
    expect(r2.text).toBe("ok");
    const opensAfterR2 = client.routeOpens.length; // 2 (E + E2)

    // R1 fails LATE — must NOT delete E2.
    releaseR1();
    await r1;

    // E2 survives: a follow-up reuses it with NO new routeOpen.
    const r3 = await t.toolCall("sess-1", "read", {});
    expect(r3.text).toBe("ok");
    expect(client.routeOpens.length).toBe(opensAfterR2); // E2 was not deleted
  });

  test("R5 #1: close during ensureClient prevents route open and bg subscription", async () => {
    let releaseConnect!: () => void;
    const connectGate = new Promise<void>((resolve) => {
      releaseConnect = resolve;
    });
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        await connectGate;
        return client;
      },
      onBgEventsNudge: () => undefined,
      bgBackoffSleep: async () => undefined,
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    const call = t.toolCall("sess-1", "read", {});
    await tick();
    await pool.closeSession(TEST_PROJECT_ROOT, "sess-1");
    releaseConnect();

    await expect(call).rejects.toBeInstanceOf(Error);
    expect(client.routeOpens.length).toBe(0);
    expect(client.subscriptions.length).toBe(0);
  });

  test("R5 #2: an old-route success still opens bg subscription after route churn", async () => {
    let releaseR1!: () => void;
    const r1Gate = new Promise<void>((resolve) => {
      releaseR1 = resolve;
    });
    let releaseR3!: () => void;
    const r3Gate = new Promise<void>((resolve) => {
      releaseR3 = resolve;
    });
    const clients: FakeClient[] = [];
    let madeClients = 0;
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        const idx = madeClients;
        let calls = 0;
        const client = new FakeClient(async () => {
          calls += 1;
          if (idx === 1) {
            if (calls === 1) {
              await r1Gate;
              return envelope({ id: "r1", success: true, text: "late success" });
            }
            throw new SocketClosedError("client A died");
          }
          if (calls === 1) {
            await r3Gate;
            return envelope({ id: "r3", success: true, text: "successor success" });
          }
          return envelope({ id: "r", success: true, text: "ok" });
        });
        clients.push(client);
        return client;
      },
      onBgEventsNudge: () => undefined,
      bgBackoffSleep: async () => undefined,
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    const r1 = t.toolCall("sess-1", "read", {});
    await tick();
    await expect(t.toolCall("sess-1", "grep", {})).rejects.toBeInstanceOf(SocketClosedError);
    const r3 = t.toolCall("sess-1", "outline", {});
    await tick();
    expect(madeClients).toBe(2);
    expect(clients[1]?.subscriptions.length).toBe(0);

    releaseR1();
    await r1;
    await tick();
    await tick();

    expect(clients[1]?.subscriptions.length).toBe(1);
    releaseR3();
    await r3;
  });

  test("R5 #3: closeSession closes the old route owner after delayed sub stop", async () => {
    let releaseStop!: () => void;
    const stopGate = new Promise<void>((resolve) => {
      releaseStop = resolve;
    });
    const oldClient = new FakeClient(async (_channel, body) => {
      if ((body as { name?: string }).name === "drop") {
        throw new SocketClosedError("old client died");
      }
      return envelope({ id: "old", success: true, text: "old" });
    });
    oldClient.subscriptionUnsubscribeGate = stopGate;
    const successor = new FakeClient(async () =>
      envelope({ id: "successor", success: true, text: "successor" }),
    );
    successor.nextChannel = 1;
    let madeClients = 0;
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        return madeClients === 1 ? oldClient : successor;
      },
      onBgEventsNudge: () => undefined,
      bgBackoffSleep: async () => undefined,
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    await t.toolCall("sess-1", "read", {});
    await tick();
    expect(oldClient.subscriptions.length).toBe(1);

    const closing = pool.closeSession(TEST_PROJECT_ROOT, "sess-1");
    await tick();
    await expect(
      pool.getBridge(TEST_OTHER_ROOT).toolCall("other", "drop", {}),
    ).rejects.toBeInstanceOf(SocketClosedError);
    await t.toolCall("sess-1", "read", {});
    expect(successor.requests[0]?.channel).toBe(1);

    releaseStop();
    await closing;

    expect(oldClient.closedRoutes).toContain(1);
    expect(successor.closedRoutes).not.toContain(1);
  });

  test("R5 #4: close-induced request failures do not trip the client failure budget", async () => {
    let releaseFailure!: () => void;
    let calls = 0;
    const client = new FakeClient(async () => {
      calls += 1;
      if (calls <= 3) {
        await new Promise<void>((resolve) => {
          releaseFailure = resolve;
        });
        throw new SubcError("route GOODBYE during close");
      }
      return envelope({ id: "r", success: true, text: "survived" });
    });
    let madeClients = 0;
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        return client;
      },
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    for (let i = 0; i < 3; i += 1) {
      const call = t.toolCall("sess-1", "edit", {}).catch((err) => err);
      await tick();
      const closing = pool.closeSession(TEST_PROJECT_ROOT, "sess-1");
      releaseFailure();
      const err = await call;
      await closing;
      expect(err).toBeInstanceOf(SubcError);
    }

    expect(madeClients).toBe(1);
    expect(client.closed).toBe(0);
    const res = await t.toolCall("sess-1", "edit", {});
    expect(res.text).toBe("survived");
    expect(madeClients).toBe(1);
  });

  test("singleflight: concurrent first calls for one identity share ONE routeOpen", async () => {
    let release!: () => void;
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    client.routeOpenGate = new Promise<void>((r) => {
      release = r;
    });
    const { pool } = poolWith(client);
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    const a = t.toolCall("sess-1", "read", {});
    const b = t.toolCall("sess-1", "grep", {});
    release();
    await Promise.all([a, b]);

    // Only ONE routeOpen despite two concurrent first calls (no leaked channel).
    expect(client.routeOpens.length).toBe(1);
    expect(client.requests[0]?.channel).toBe(client.requests[1]?.channel);
  });

  test("tombstone: closeSession during an in-flight routeOpen self-closes the route", async () => {
    let release!: () => void;
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    client.routeOpenGate = new Promise<void>((r) => {
      release = r;
    });
    const { pool } = poolWith(client);
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    const call = t.toolCall("sess-1", "read", {}).catch((e) => e);
    await tick(); // let routeOpen start (now gated)
    const close = pool.closeSession(TEST_PROJECT_ROOT, "sess-1");
    release();
    const [err] = await Promise.all([call, close]);

    // The racing open resolved AFTER teardown → channel closed, not cached, call failed.
    expect(err).toBeInstanceOf(Error);
    expect(client.closedRoutes.length).toBe(1);
  });

  test("R2-T1: a stale opener resolving after closeSession does not delete a newer route", async () => {
    // call A opens (gated) → closeSession tombstones A's entry → call B opens a
    // fresh route → A resolves, self-closes, and must NOT delete B's entry.
    let releaseA!: () => void;
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    client.routeOpenGate = new Promise<void>((r) => {
      releaseA = r;
    });
    const { pool } = poolWith(client);
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    const callA = t.toolCall("sess-1", "read", {}).catch((e) => e);
    await tick(); // A's routeOpen is now gated
    await pool.closeSession(TEST_PROJECT_ROOT, "sess-1"); // Mark route A as reclaimed so its late completion can close only its own channel without affecting the newer route opened by call B.

    // call B (same identity) opens a fresh route and succeeds.
    client.routeOpenGate = null; // B opens immediately
    const resB = await t.toolCall("sess-1", "grep", {});
    expect(resB.success).toBe(true);

    // Now A resolves — it must self-close its own channel and leave B's intact.
    releaseA();
    await callA;

    // B's route is still tracked: a follow-up call reuses it (no new routeOpen).
    const openCountBefore = client.routeOpens.length;
    await t.toolCall("sess-1", "outline", {});
    expect(client.routeOpens.length).toBe(openCountBefore); // B's entry survived
  });

  test("R2-T2: the half-open counter does not carry across client generations", async () => {
    // Client A: 2 non-transient timeouts (counter=2), then a TRANSIENT socket drop
    // (replaces A with B). B's first non-transient timeout must NOT trip the
    // backstop — the counter resets on the client swap, so B is kept and recovers.
    let madeClients = 0;
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        const idx = madeClients;
        let calls = 0;
        return new FakeClient(async () => {
          calls += 1;
          if (idx === 1) {
            // client A: two non-transient timeouts, then a transient socket death
            if (calls <= 2) throw new SubcError("A timed out");
            throw new SocketClosedError("A socket closed"); // transient → drop A
          }
          // client B: first call non-transient timeout, then succeeds
          if (calls === 1) throw new SubcError("B timed out");
          return envelope({ id: "r", success: true, text: "B-ok" });
        });
      },
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    // Two non-transient timeouts on A → counter = 2 (A kept, not yet 3).
    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    expect(madeClients).toBe(1);

    // Third call hits A's transient socket death → A dropped, counter reset.
    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SocketClosedError);

    // B's first call is a non-transient timeout — if the counter had carried A's
    // 2, this would be the 3rd and drop B. It must NOT: B is kept.
    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    expect(madeClients).toBe(2); // still B, not a 3rd client
    // B then succeeds — proving B survived its first failure (no carryover).
    const res = await t.toolCall("s", "edit", {});
    expect(res.text).toBe("B-ok");
    expect(madeClients).toBe(2);
  });

  test("R3: a late failure from a REPLACED client does not corrupt the new client's state", async () => {
    // R1 is in flight on client A (held). A second request transient-fails and
    // drops A; a third installs client B. Then R1 fails LATE on the dead A — its
    // catch must NOT touch B's route cache / failure budget (stale generation).
    let releaseR1!: () => void;
    const r1Gate = new Promise<void>((r) => {
      releaseR1 = r;
    });
    const clients: FakeClient[] = [];
    let madeClients = 0;
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        const idx = madeClients;
        let calls = 0;
        const c = new FakeClient(async () => {
          calls += 1;
          if (idx === 1) {
            if (calls === 1) {
              await r1Gate; // R1 held, then fails late, non-transiently
              throw new SubcError("R1 late timeout");
            }
            throw new SocketClosedError("A dead"); // R2 transient → drops A
          }
          return envelope({ id: "r", success: true, text: "B-ok" }); // client B
        });
        clients.push(c);
        return c;
      },
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    const r1 = t.toolCall("s", "read", {}).catch((e) => e); // in flight on A
    await tick();
    await expect(t.toolCall("s", "read", {})).rejects.toBeInstanceOf(SocketClosedError); // drops A
    const r3 = await t.toolCall("s", "read", {}); // installs + uses B
    expect(r3.text).toBe("B-ok");
    expect(madeClients).toBe(2);
    const bRouteOpensAfterR3 = clients[1]?.routeOpens.length;

    // R1 fails LATE on the dead client A — must be a no-op against B's state.
    releaseR1();
    await r1;

    const r4 = await t.toolCall("s", "read", {});
    expect(r4.text).toBe("B-ok");
    expect(madeClients).toBe(2); // R1's stale failure did NOT drop/replace B
    // B's cached route survived (no extra routeOpen): R1 didn't delete it.
    expect(clients[1]?.routeOpens.length).toBe(bRouteOpensAfterR3);
  });

  test("a transient routeOpen failure drops the client so the next call reconnects", async () => {
    let madeClients = 0;
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        const c = new FakeClient(async () => envelope({ id: "r", success: true, text: "ok" }));
        if (madeClients === 1) c.routeOpenError = new SocketClosedError("dead");
        return c;
      },
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    await expect(t.toolCall("s", "read", {})).rejects.toBeInstanceOf(SocketClosedError);
    const res = await t.toolCall("s", "read", {});
    expect(res.text).toBe("ok");
    expect(madeClients).toBe(2); // dead client dropped on the routeOpen failure
  });

  test("half-open backstop: 3 consecutive non-transient throws force a reconnect", async () => {
    let madeClients = 0;
    let calls = 0;
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        return new FakeClient(async () => {
          calls += 1;
          if (calls <= 3) throw new SubcError("timed out");
          return envelope({ id: "r", success: true, text: "recovered" });
        });
      },
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    // Three non-transient timeouts: client kept for the first two, dropped on the third.
    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    expect(madeClients).toBe(1); // not yet reconnected mid-run
    const res = await t.toolCall("s", "edit", {});
    expect(res.text).toBe("recovered");
    expect(madeClients).toBe(2); // 3rd failure tripped the reconnect
  });

  test("a success between failures resets the half-open counter", async () => {
    let madeClients = 0;
    let calls = 0;
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        return new FakeClient(async () => {
          calls += 1;
          // fail, fail, succeed, fail, fail — never 3 in a row → never reconnects.
          if (calls === 3) return envelope({ id: "r", success: true, text: "ok" });
          throw new SubcError("timed out");
        });
      },
    });
    const t = pool.getBridge(TEST_PROJECT_ROOT);
    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    await t.toolCall("s", "edit", {}); // success resets the counter
    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    expect(madeClients).toBe(1); // run was 2-1-2, never 3 consecutive → no reconnect
  });

  test("shutdown during an in-flight connect closes the late client (no leak)", async () => {
    let release!: (c: FakeClient) => void;
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: () =>
        new Promise<SubcClientLike>((r) => {
          release = r as (c: FakeClient) => void;
        }),
    });

    const call = pool
      .getBridge(TEST_PROJECT_ROOT)
      .toolCall("s", "read", {})
      .catch((e) => e);
    await tick(); // connect now in flight
    await pool.shutdown();
    release(client); // connect resolves AFTER shutdown
    await call;

    expect(client.closed).toBe(1); // the late client was closed, not installed
  });
});

describe("SubcTransport.send", () => {
  test("configure is satisfied locally and never hits the wire", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);

    const res = await pool
      .getBridge(TEST_PROJECT_ROOT)
      .send("configure", { project_root: TEST_PROJECT_ROOT });
    expect(res.success).toBe(true);
    expect(res.subc_local).toBe(true);
    expect(client.requests.length).toBe(0); // no route request issued
  });

  test("a native command rides the route as {name, arguments} scoped to its session", async () => {
    const client = new FakeClient(async () =>
      envelope({ id: "r", success: true, text: "", bg_completions: [] }),
    );
    const { pool } = poolWith(client);

    await pool
      .getBridge(TEST_PROJECT_ROOT)
      .send("bash_drain_completions", { session_id: "sess-Z" });

    expect(client.routeOpens[0]?.session).toBe("sess-Z");
    expect(client.requests[0]?.body).toEqual({
      name: "bash_drain_completions",
      arguments: { session_id: "sess-Z" },
    });
  });
});

describe("SubcTransport bg_events subscription (S3)", () => {
  let previousLogger: Logger | undefined;
  let lifecycleLogs: Array<{ message: string; meta?: LogMeta }>;

  beforeEach(() => {
    previousLogger = getActiveLogger();
    lifecycleLogs = [];
    setActiveLogger({
      log: (message, meta) => lifecycleLogs.push({ message, meta }),
      warn: () => undefined,
      error: () => undefined,
    });
  });

  afterEach(() => {
    setActiveLogger(previousLogger as Logger);
  });

  function bgPool(client: FakeClient): {
    pool: SubcTransportPool;
    nudges: { root: string; session: string }[];
  } {
    const nudges: { root: string; session: string }[] = [];
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => client,
      onBgEventsNudge: (root, session) => nudges.push({ root, session }),
      bgBackoffSleep: async () => undefined, // no real delay in tests
    });
    return { pool, nudges };
  }

  test("opens a dedicated bg subscription on a DISTINCT channel after the first tool call", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = bgPool(client);

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {});
    await tick();

    // Two route.opens: the tool route + the dedicated bg_events route.
    expect(client.routeOpens.length).toBe(2);
    expect(client.subscriptions.length).toBe(1);
    // The bg subscription rides a DIFFERENT channel from the tool request.
    const toolChannel = client.requests[0]?.channel;
    expect(client.subscriptions[0]?.channel).not.toBe(toolChannel);
  });

  test("a nudge AND the initial (re)subscribe both fire onBgEventsNudge (forced-drain trigger)", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool, nudges } = bgPool(client);

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {});
    await tick();
    // Immediate replay nudge on subscribe.
    expect(nudges.length).toBe(1);

    // A wake nudge from the module drives another drain.
    client.subscriptions[0]?.emit();
    expect(nudges.length).toBe(2);
    expect(nudges[1]).toEqual({
      root: pool.getBridge(TEST_PROJECT_ROOT).getCwd(),
      session: "sess-1",
    });
    client.subscriptions[0]?.emit();
    expect(nudges.length).toBe(3);
    expect(lifecycleLogs.filter((entry) => entry.message.includes("nudge received"))).toEqual([
      {
        message: "subc bg_events: nudge received channel=2@2 count=1",
        meta: { sessionId: "sess-1" },
      },
    ]);
  });

  test("falls back to the root/session handler when generation provenance is unavailable", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const nudges: Array<{ root: string; session: string }> = [];
    const refs: unknown[] = [];
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => client,
      onBgEventsNudge: (root, session) => nudges.push({ root, session }),
      onBgEventsNudgeRef: (ref) => refs.push(ref),
      bgBackoffSleep: async () => undefined,
    });

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {});
    await tick();
    // The first tool call triggers a setup replay; assert only the emitted wake below.
    nudges.length = 0;
    client.subscriptions[0]?.emit();

    expect(nudges).toEqual([
      {
        root: pool.getBridge(TEST_PROJECT_ROOT).getCwd(),
        session: "sess-1",
      },
    ]);
    expect(refs).toHaveLength(0);
    expect(lifecycleLogs.map((entry) => entry.message)).toContain(
      `subc bg_events: nudge dispatch fallback=root-session-handler cause=generation-provenance-unavailable root=${pool.getBridge(TEST_PROJECT_ROOT).getCwd()}`,
    );
    await pool.shutdown();
  });

  test("delivers a nudge carried by a superseded record to the current session record", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool, nudges } = bgPool(client);

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {});
    await tick();
    // The first tool call triggers a setup replay; assert only the emitted wake below.
    nudges.length = 0;

    const internals = pool as unknown as {
      sessions: Map<
        string,
        {
          bgSub: { stop(): Promise<void> } | null;
          closed: boolean;
        }
      >;
    };
    const [key, carryingRecord] = [...internals.sessions.entries()][0]!;
    const carryingSubscription = carryingRecord.bgSub;
    const currentRecord = { ...carryingRecord, bgSub: null, closed: false };
    internals.sessions.set(key, currentRecord);

    try {
      client.subscriptions[0]?.emit();

      expect(nudges).toEqual([
        {
          root: pool.getBridge(TEST_PROJECT_ROOT).getCwd(),
          session: "sess-1",
        },
      ]);
      expect(lifecycleLogs.map((entry) => entry.message)).toContain(
        `subc bg_events: nudge forwarding cause=superseded-carrying-record root=${pool.getBridge(TEST_PROJECT_ROOT).getCwd()}`,
      );
    } finally {
      await carryingSubscription?.stop();
      await pool.shutdown();
    }
  });

  test("logs subc-client epoch-guard drops while a quiet subscription remains open", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => client,
      onBgEventsNudge: () => undefined,
      bgBackoffSleep: async () => undefined,
      bgDispatchProbeIntervalMs: 1,
    });

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("epoch-drop", "read", {});
    await tick();
    client.droppedIngressFrames = 2;
    await new Promise((resolve) => setTimeout(resolve, 5));

    expect(lifecycleLogs.map((entry) => entry.message)).toContain(
      "subc bg_events: client ingress epoch drops scope=client observed_while_channel=2@2 delta=2 total=2",
    );
    await pool.shutdown();
  });

  test("is idempotent — one subscription per session even across many tool calls", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = bgPool(client);
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    await t.toolCall("sess-1", "read", {});
    await tick();
    await t.toolCall("sess-1", "grep", {});
    await t.toolCall("sess-1", "edit", {});
    await tick();

    expect(client.subscriptions.length).toBe(1); // never re-subscribed for the same session
  });

  test("R4 #1: a late success after closeSession does NOT resurrect the bg subscription", async () => {
    // A tool call is in flight; closeSession tears down the session mid-flight; the
    // request then succeeds LATE. Its success path must NOT re-open a bg_events
    // subscription for the just-closed session (zombie route + nudges + leak).
    let releaseReq!: () => void;
    const gate = new Promise<void>((r) => {
      releaseReq = r;
    });
    let calls = 0;
    const client = new FakeClient(async () => {
      calls += 1;
      if (calls === 1) await gate; // first tool call held in flight
      return envelope({ id: "r", success: true, text: "" });
    });
    const { pool } = bgPool(client);
    const t = pool.getBridge(TEST_PROJECT_ROOT);

    const inflight = t.toolCall("sess-1", "read", {}); // held on `gate`
    await tick();
    expect(client.subscriptions.length).toBe(0); // not subscribed yet (reply pending)

    // Close the session WHILE the request is in flight.
    await pool.closeSession(TEST_PROJECT_ROOT, "sess-1");

    // Now let the held request succeed LATE.
    releaseReq();
    await inflight;
    await tick();

    // The late success must NOT have created a subscription for the closed session.
    expect(client.subscriptions.length).toBe(0);
  });

  test("INDEPENDENT reconnect: a dropped subscription resubscribes + re-drains with NO tool call (idle-stranding fix)", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool, nudges } = bgPool(client);

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {});
    await tick();
    expect(nudges.length).toBe(1); // initial subscribe replay
    const firstSub = client.subscriptions[0];

    // Socket drop — NO tool call follows (idle agent). The loop must resubscribe.
    firstSub?.drop();
    await tick();
    await tick();

    expect(client.subscriptions.length).toBe(2); // resubscribed independently
    // The resubscribe fired another forced-drain replay (recovers a completion
    // that landed while disconnected).
    expect(nudges.length).toBe(2);
    expect(lifecycleLogs.map((entry) => entry.message)).toContain(
      "subc bg_events: stream error channel=2@2 error=Error: subscription dropped",
    );
    expect(lifecycleLogs.map((entry) => entry.message)).toContain(
      "subc bg_events: reconnect attempt=1",
    );
    expect(lifecycleLogs.map((entry) => entry.message)).toContain(
      "subc bg_events: reconnect success attempt=1 channel=3@3",
    );
  });

  test("B-#1: a TRANSIENT subscription drop replaces the dead client before resubscribe", async () => {
    // Two clients from the factory; the bg loop must drop the dead one and
    // reconnect, not resubscribe forever onto the same dead socket (idle-stranding).
    const clients: FakeClient[] = [];
    let madeClients = 0;
    const nudges: { root: string; session: string }[] = [];
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        const c = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
        clients.push(c);
        return c;
      },
      onBgEventsNudge: (root, session) => nudges.push({ root, session }),
      bgBackoffSleep: async () => undefined,
    });

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {});
    await tick();
    expect(madeClients).toBe(1);

    // Dead-connection drop (transient): the bg loop must drop client #1 and
    // reconnect via a fresh client #2, then resubscribe there.
    clients[0]?.subscriptions[0]?.dropTransient();
    await tick();
    await tick();

    expect(madeClients).toBe(2); // reconnected, not stranded on the dead client
    expect(clients[1]?.subscriptions.length).toBe(1); // resubscribed on the new client
  });

  test("B-#2: the dedicated bg route is closed on the drop→resubscribe path (no leak)", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = bgPool(client);

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {});
    await tick();
    const firstBgChannel = client.subscriptions[0]?.channel;

    client.subscriptions[0]?.drop(); // non-transient: keep client, re-open route
    await tick();
    await tick();

    // The first bg route was closed (finally), and a new one opened.
    expect(client.closedRoutes).toContain(firstBgChannel);
    expect(client.subscriptions.length).toBe(2);
  });

  test("provider StreamEnd resubscribes and logs lifecycle while the session remains live", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = bgPool(client);

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {});
    await tick();
    expect(lifecycleLogs[0]).toEqual({
      message: "subc bg_events: subscription open channel=2@2",
      meta: { sessionId: "sess-1" },
    });

    client.subscriptions[0]?.end();
    await tick();
    await tick();

    expect(client.subscriptions.length).toBe(2);
    expect(lifecycleLogs.map((entry) => entry.message)).toEqual([
      "subc bg_events: subscription open channel=2@2",
      "subc bg_events: stream ended channel=2@2",
      "subc bg_events: reconnect attempt=1",
      "subc bg_events: reconnect success attempt=1 channel=3@3",
    ]);
  });

  test("restart burst parks all bg reconnects without retaining shared ingress listeners", async () => {
    const sessionCount = 8;
    const clock = new FakeClock();
    const clients: FakeClient[] = [];
    let acceptingConnections = true;
    let connectAttempts = 0;
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        connectAttempts += 1;
        if (!acceptingConnections) throw new SocketClosedError("daemon is restarting");
        const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
        clients.push(client);
        return client;
      },
      onBgEventsNudge: () => undefined,
      bgBackoffSleep: clock.sleep,
    });
    const transport = pool.getBridge(TEST_PROJECT_ROOT);

    await Promise.all(
      Array.from({ length: sessionCount }, (_, index) =>
        transport.toolCall(`restart-${index}`, "read", {}),
      ),
    );
    await settleMicrotasks();

    const firstClient = clients[0]!;
    const initialRouteOpens = firstClient.routeOpens.length;
    const baselineIngressListeners = firstClient.ingressListenerCount;
    expect(baselineIngressListeners).toBe(sessionCount);

    // A module restart ends every held route together. While it is unavailable,
    // reopening routes reports a socket failure and later connects fail quickly.
    acceptingConnections = false;
    firstClient.routeOpenFailure = () => new SocketClosedError("daemon is restarting");
    for (const subscription of firstClient.subscriptions.slice(0, sessionCount)) subscription.end();
    await settleMicrotasks();

    expect(firstClient.ingressListenerCount).toBe(0);
    expect(clock.scheduledWorkCount).toBe(sessionCount);
    await settleMicrotasks();
    expect(clock.scheduledWorkCount).toBe(sessionCount);

    // The first wave probes the retired client once per session. Later waves share
    // ensureClient's single-flight connect and follow the 100ms-to-2s schedule.
    for (const delay of [100, 200, 400, 800, 1_600, 2_000, 2_000]) {
      await clock.advance(delay);
      expect(clock.scheduledWorkCount).toBe(sessionCount);
    }
    expect(firstClient.routeOpens.length - initialRouteOpens).toBe(sessionCount);
    expect(connectAttempts - 1).toBe(6);
    expect([...new Set(clock.scheduledDelays)]).toEqual([100, 200, 400, 800, 1_600, 2_000]);

    acceptingConnections = true;
    await clock.advance(2_000);

    const recoveredClient = clients[1]!;
    expect(recoveredClient.subscriptions).toHaveLength(sessionCount);
    expect(recoveredClient.ingressListenerCount).toBe(baselineIngressListeners);
    expect(clock.scheduledWorkCount).toBe(0);

    await pool.shutdown();
    await settleMicrotasks();
    expect(recoveredClient.ingressListenerCount).toBe(0);
  });

  test("closeSession stops the subscription and closes both routes", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = bgPool(client);

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {});
    await tick();
    await pool.closeSession(TEST_PROJECT_ROOT, "sess-1");

    expect(client.subscriptions[0]?.unsubscribed).toBe(1);
    // Both the bg route and the tool route were closed.
    expect(client.closedRoutes.length).toBe(2);
    expect(lifecycleLogs.map((entry) => entry.message)).toContain(
      "subc bg_events: reconnect gave-up attempt=0 reason=stopped",
    );
  });

  test("no bg subscription is opened when onBgEventsNudge is not configured", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client); // no onBgEventsNudge

    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("sess-1", "read", {});
    await tick();

    expect(client.subscriptions.length).toBe(0);
    expect(client.routeOpens.length).toBe(1); // tool route only
  });
});

describe("SubcTransportPool lifecycle", () => {
  test("getActiveBridgeForRoot returns null before connect, a transport after", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);

    expect(pool.getActiveBridgeForRoot(TEST_PROJECT_ROOT)).toBeNull();
    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("s", "read", {});
    expect(pool.getActiveBridgeForRoot(TEST_PROJECT_ROOT)).not.toBeNull();
  });

  test("shutdown closes the client and rejects further calls", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);
    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("s", "read", {});

    await pool.shutdown();
    expect(client.closed).toBe(1);
    await expect(
      pool.getBridge(TEST_PROJECT_ROOT).toolCall("s", "read", {}),
    ).rejects.toBeInstanceOf(SubcCallError);
  });

  test("setConfigureOverride and replaceBinary are no-ops over subc", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);
    expect(() => pool.setConfigureOverride("k", "v")).not.toThrow();
    await expect(pool.replaceBinary("/new/path")).resolves.toBe("/new/path");
  });
});

describe("subc AbortSignal transport", () => {
  test("closes the scoped route once and rejects promptly when the host aborts", async () => {
    let markRequestStarted!: () => void;
    const requestStarted = new Promise<void>((resolve) => {
      markRequestStarted = resolve;
    });
    const client = new FakeClient(async () => {
      markRequestStarted();
      return new Promise(() => {});
    });
    const transport = poolWith(client);
    const { pool } = transport;
    const controller = new AbortController();
    const request = pool.getBridge(TEST_PROJECT_ROOT).toolCall(
      "session-abort",
      "aft_search",
      { query: "needle" },
      {
        abortSignal: controller.signal,
      },
    );

    await requestStarted;
    controller.abort();

    await expect(request).rejects.toMatchObject({ name: "AbortError" });
    expect(client.closedRoutes).toEqual([1]);
    controller.abort();
    await tick();
    expect(client.closedRoutes).toEqual([1]);

    for (const expectedRoute of [2, 3]) {
      const nextController = new AbortController();
      const nextRequest = pool.getBridge(TEST_PROJECT_ROOT).toolCall(
        "session-abort",
        "aft_search",
        { query: "needle" },
        {
          abortSignal: nextController.signal,
        },
      );
      await tick();
      nextController.abort();
      await expect(nextRequest).rejects.toMatchObject({ name: "AbortError" });
      expect(client.closedRoutes.at(-1)).toBe(expectedRoute);
    }
    expect(transport.connects).toBe(1);

    await pool.shutdown();
  });

  test("does not dispatch a Rust call when the signal is already aborted", async () => {
    const client = new FakeClient(async () => envelope({ success: true, text: "unexpected" }));
    const { pool } = poolWith(client);
    const controller = new AbortController();
    controller.abort();

    await expect(
      pool
        .getBridge(TEST_PROJECT_ROOT)
        .send("aft_search", { query: "needle" }, { abortSignal: controller.signal }),
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(client.requests).toHaveLength(0);
    expect(client.closedRoutes).toEqual([1]);

    await pool.shutdown();
  });
});
