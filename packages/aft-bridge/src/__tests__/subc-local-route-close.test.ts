/**
 * The plugin closing one of its own routes while a call is in flight on it:
 * pool shutdown (which is also what host-process exit runs), session close,
 * project-root close, and replacement of the session's shared route.
 *
 * Exercised through a REAL `SubcClient` over a real TCP socket against a small
 * in-process fake subc daemon. The daemon records every data-plane request it
 * receives and holds until explicitly released, so a call is provably in flight (its request
 * reached the daemon) when the close happens. A test double that resolved or
 * rejected requests by itself could not show that, and would hide whether the
 * real client's `closeRoute` rejection is what the caller sees.
 */

import { afterEach, describe, expect, spyOn, test } from "bun:test";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  buildFlags,
  buildFrame,
  computeProof,
  decodeHeader,
  encodeFrame,
  FrameType,
  HEADER_LEN,
  Priority,
  SERVER_PROOF_DOMAIN,
  SubcError,
} from "@cortexkit/subc-client";
import {
  adaptToolError,
  BASH_TRANSPORT_DISPOSITION,
  classifyBashHostFallbackError,
  SUBC_CONNECTION_CLOSED_MID_CALL_DISPOSITION,
  SUBC_MODULE_RESTART_DISPOSITION,
  SUBC_ROUTE_CLOSED_MID_CALL_DISPOSITION,
} from "../error-contract.js";
import { asCanonicalRootPath, LifecycleRegistry } from "../lifecycle-registry.js";
import {
  isSubcRouteTornDownError,
  SubcRouteClosedMidCallError,
  SubcTransportPool,
  SubcTransportShuttingDownError,
} from "../subc-transport.js";
import { TEST_INFLIGHT_ROOT, TEST_PROJECT_ROOT } from "./subc-test-roots.js";
import { ManualClock } from "./test-utils/manual-clock.js";

interface FakeDaemon {
  connectionFile: string;
  /** Data-plane request bodies the daemon received, in arrival order. */
  readonly requests: Array<{ name?: string; channel: number; epoch: number }>;
  readonly closedRoutes: number[];
  readonly catalogProbes: number;
  readonly connections: number;
  /** A terminal releases exactly one request credit, only on a still-live route. */
  respond(index: number, body?: unknown): boolean;
  fail(index: number, code: string, message: string): boolean;
  loseConnections(): void;
  /** While true, route.open requests are held instead of answered. */
  holdRouteOpens: boolean;
  /** Route opens received while held. */
  readonly heldRouteOpens: number;
  /** Answer every held route.open, accepting it. */
  releaseRouteOpens(): void;
  /** Data-plane requests are answered with this body; null holds them forever. */
  reply: unknown;
  close(): Promise<void>;
}

async function startFakeDaemon(): Promise<FakeDaemon> {
  const key = new Uint8Array(32).fill(7);
  const daemonId = new Uint8Array(16).fill(9);
  const sockets = new Set<Socket>();
  const requests: Array<{ name?: string; channel: number; epoch: number }> = [];
  const terminals: Array<(ty: FrameType, body: unknown) => boolean> = [];
  const closedRoutes: number[] = [];
  let catalogProbes = 0;
  let connections = 0;
  const heldOpens: Array<() => void> = [];
  let nextChannel = 1;
  const control = {
    holdRouteOpens: false,
    reply: null as unknown,
  };

  const server: Server = createServer((socket) => {
    connections += 1;
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => undefined);
    let buffered = Buffer.alloc(0);
    // Handshake messages are 4-byte little-endian length + JSON; after the
    // client's auth message the stream switches to envelope frames.
    let phase: "hello" | "auth" | "frames" = "hello";
    const routes = new Set<number>();
    // Like the daemon, Cancel is not a terminal and cannot release a credit.
    // GOODBYE destroys the route: a delayed provider cannot resurrect it.
    const credits = new Set<string>();

    const writeAuthMessage = (value: unknown): void => {
      const json = Buffer.from(JSON.stringify(value), "utf8");
      const prefix = Buffer.alloc(4);
      prefix.writeUInt32LE(json.length, 0);
      socket.write(Buffer.concat([prefix, json]));
    };
    const send = (
      ty: FrameType,
      channel: number,
      epoch: number,
      corr: bigint,
      body: unknown,
    ): void => {
      const bytes =
        body === null ? new Uint8Array(0) : new Uint8Array(Buffer.from(JSON.stringify(body)));
      const frame = buildFrame(
        ty,
        buildFlags(false, Priority.Interactive, false),
        channel,
        epoch,
        corr,
        bytes,
      );
      if (!socket.destroyed) socket.write(encodeFrame(frame));
    };

    socket.on("data", (chunk: Buffer) => {
      buffered = Buffer.concat([buffered, chunk]);
      for (;;) {
        if (phase !== "frames") {
          if (buffered.length < 4) return;
          const len = buffered.readUInt32LE(0);
          if (buffered.length < 4 + len) return;
          const message = JSON.parse(buffered.subarray(4, 4 + len).toString("utf8")) as {
            client_nonce?: number[];
          };
          buffered = buffered.subarray(4 + len);
          if (phase === "hello") {
            const clientNonce = Uint8Array.from(message.client_nonce ?? []);
            const serverNonce = new Uint8Array(32).fill(3);
            writeAuthMessage({
              daemon_id: Array.from(daemonId),
              server_nonce: Array.from(serverNonce),
              daemon_ver: "fake",
              server_proof: Array.from(
                computeProof(key, SERVER_PROOF_DOMAIN, clientNonce, serverNonce, daemonId),
              ),
            });
            // The client's auth proof is not verified: this daemon only exists
            // to script answers for the client under test.
            phase = "auth";
          } else {
            phase = "frames";
          }
          continue;
        }

        if (buffered.length < HEADER_LEN) return;
        const header = decodeHeader(new Uint8Array(buffered.subarray(0, HEADER_LEN)));
        if (buffered.length < HEADER_LEN + header.len) return;
        const body = buffered.subarray(HEADER_LEN, HEADER_LEN + header.len);
        buffered = buffered.subarray(HEADER_LEN + header.len);

        if (header.ty === FrameType.Ping) {
          send(FrameType.Pong, header.channel, header.epoch, header.corr, null);
          continue;
        }
        if (header.ty === FrameType.Goodbye) {
          routes.delete(header.channel);
          closedRoutes.push(header.channel);
          continue;
        }
        if (header.ty !== FrameType.Request) continue;

        if (header.channel === 0) {
          const op = (JSON.parse(body.toString("utf8")) as { op?: string }).op;
          if (op === "catalog.list") {
            catalogProbes += 1;
            send(FrameType.Response, 0, 0, header.corr, { modules: [] });
            continue;
          }
          if (op !== "route.open") {
            send(FrameType.Error, 0, 0, header.corr, {
              code: "unsupported",
              message: `fake daemon does not implement ${op}`,
            });
            continue;
          }
          const accept = (): void => {
            const channel = nextChannel++;
            routes.add(channel);
            send(FrameType.Response, 0, 0, header.corr, {
              op: "route.open",
              route_channel: channel,
              route_epoch: 1,
            });
          };
          if (control.holdRouteOpens) heldOpens.push(accept);
          else accept();
          continue;
        }

        if (!routes.has(header.channel)) {
          send(FrameType.Error, header.channel, header.epoch, header.corr, {
            code: "unknown_channel",
            message: "route is closed",
          });
          continue;
        }
        requests.push({
          ...(JSON.parse(body.toString("utf8")) as { name?: string }),
          channel: header.channel,
          epoch: header.epoch,
        });
        const credit = `${header.channel}@${header.epoch}/${header.corr}`;
        if (credits.has(credit)) throw new Error(`duplicate request credit ${credit}`);
        credits.add(credit);
        const terminal = (ty: FrameType, reply: unknown): boolean => {
          if (socket.destroyed || !routes.has(header.channel) || !credits.delete(credit))
            return false;
          send(ty, header.channel, header.epoch, header.corr, reply);
          return true;
        };
        terminals.push(terminal);
        if (control.reply !== null) {
          terminal(FrameType.Response, control.reply);
        }
      }
    });
  });

  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("no tcp address");

  const dir = mkdtempSync(join(tmpdir(), "aft-fake-subc-"));
  const connectionFile = join(dir, "connection.json");
  writeFileSync(
    connectionFile,
    JSON.stringify({
      schema: 1,
      wire_version: 2,
      endpoints: [{ host: "127.0.0.1", port: address.port }],
      key: Array.from(key),
      daemon_id: Array.from(daemonId),
      pid: process.pid,
      daemon_ver: "fake",
    }),
  );
  // subc-client refuses a connection file other users could read.
  chmodSync(connectionFile, 0o600);

  return {
    connectionFile,
    requests,
    closedRoutes,
    get catalogProbes() {
      return catalogProbes;
    },
    get connections() {
      return connections;
    },
    respond(index, body = LIVE_REPLY) {
      return terminals[index]?.(FrameType.Response, body) ?? false;
    },
    fail(index, code, message) {
      return terminals[index]?.(FrameType.Error, { code, message }) ?? false;
    },
    loseConnections() {
      for (const socket of sockets) socket.destroy();
    },
    get holdRouteOpens() {
      return control.holdRouteOpens;
    },
    set holdRouteOpens(value: boolean) {
      control.holdRouteOpens = value;
    },
    get heldRouteOpens() {
      return heldOpens.length;
    },
    releaseRouteOpens() {
      control.holdRouteOpens = false;
      for (const accept of heldOpens.splice(0)) accept();
    },
    get reply() {
      return control.reply;
    },
    set reply(value: unknown) {
      control.reply = value;
    },
    async close() {
      for (const socket of sockets) socket.destroy();
      await new Promise<void>((resolve) => server.close(() => resolve()));
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

const LIVE_REPLY = {
  content: [{ type: "text", text: "live" }],
  isError: false,
  structuredContent: { id: "r", success: true, text: "live" },
};

async function waitFor(condition: () => boolean, what: string): Promise<void> {
  const deadline = Date.now() + 5_000;
  while (!condition()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

/**
 * The agent-facing outcome of an in-flight bash call cut off by a local close:
 * no host fallback, SUBC_ROUTE_CLOSED_MID_CALL_DISPOSITION (outcome unknown),
 * and neither BASH_TRANSPORT_DISPOSITION (which says to re-run) nor
 * SUBC_MODULE_RESTART_DISPOSITION (which blames a daemon module restart).
 */
function expectUnknownOutcomeForBash(error: unknown, reason: string): void {
  expect(error).toBeInstanceOf(SubcRouteClosedMidCallError);
  const midCall = error as SubcRouteClosedMidCallError;
  expect(midCall.reason).toBe(reason);
  expect(midCall.kind).toBe("outcome_unknown");
  expect(midCall.message).toContain(
    `the command may have run; the route closed mid-call: ${reason}`,
  );
  // The pool keeps subc-client's own rejection as the cause, and recognises it
  // by the client's typed close reason rather than its message text.
  expect(midCall.cause).toBeInstanceOf(SubcError);
  expect((midCall.cause as SubcError).closeReason).toBe("closed_by_caller");

  expect(classifyBashHostFallbackError(error)).toBeUndefined();
  const adapted = adaptToolError("bash", error) as Error;
  expect(adapted.message).toContain(SUBC_ROUTE_CLOSED_MID_CALL_DISPOSITION);
  expect(adapted.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
  expect(adapted.message).not.toContain(SUBC_MODULE_RESTART_DISPOSITION);
}

describe("a route the plugin closes under an in-flight call (real SubcClient, fake daemon)", () => {
  const cleanups: Array<() => Promise<void> | void> = [];
  afterEach(async () => {
    while (cleanups.length > 0) await cleanups.pop()?.();
  });

  async function rig(options: { lifecycle?: LifecycleRegistry } = {}): Promise<{
    pool: SubcTransportPool;
    daemon: FakeDaemon;
  }> {
    const daemon = await startFakeDaemon();
    cleanups.push(() => daemon.close());
    const pool = new SubcTransportPool({
      connectionFile: daemon.connectionFile,
      harness: "opencode",
      consumerIdentity: null,
      handshakeTimeoutMs: 2_000,
      ...(options.lifecycle
        ? {
            lifecycleRegistry: options.lifecycle,
            lifecycleDemandCheck: () => true,
            reapingEnabled: true,
          }
        : {}),
    });
    cleanups.push(() => pool.shutdown());
    return { pool, daemon };
  }

  test("a real SubcClient request keeps its correlation through sleep and accepts the grace reply", async () => {
    const { pool, daemon } = await rig();
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    daemon.reply = LIVE_REPLY;
    await bridge.send("read", { session_id: "sdk-sleep" });
    daemon.reply = null;
    const sdk = (
      pool as unknown as {
        client: { pending: Map<string, { sleepGrace?: { used: boolean } }> };
      }
    ).client;
    const previous = new Set(sdk.pending.keys());
    const nativeNow = Date.now.bind(Date);
    const nativeTimeout = globalThis.setTimeout;
    const nativeClearTimeout = globalThis.clearTimeout;
    const nativeWaitFor = async (condition: () => boolean) => {
      const cutoff = nativeNow() + 2_000;
      while (!condition()) {
        if (nativeNow() >= cutoff) throw new Error("real SDK did not dispatch the expected event");
        await new Promise<void>((resolve) => nativeTimeout(resolve, 1));
      }
    };
    const clock = new ManualClock();
    // Keep TCP and the SDK's setImmediate arbitration real. Only its wall,
    // awake and timeout sources are simulated; the SDK owns the correlation.
    const wall = spyOn(Date, "now").mockImplementation(() => clock.now());
    const awake = spyOn(performance, "now").mockImplementation(() => clock.awakeNow());
    const timers = spyOn(globalThis, "setTimeout").mockImplementation(
      ((callback: () => void, ms: number) =>
        -(clock.setTimeout(callback, ms) as number)) as typeof setTimeout,
    );
    const clears = spyOn(globalThis, "clearTimeout").mockImplementation((timer) => {
      if (typeof timer === "number" && timer < 0) clock.clearTimeout(-timer);
      else nativeClearTimeout(timer);
    });
    try {
      let settlements = 0;
      const call = bridge.send(
        "bash",
        { session_id: "sdk-sleep", command: "start once" },
        { transportTimeoutMs: 1_000 },
      );
      const outcome = call.then(
        (value) => {
          settlements++;
          return { value };
        },
        (error) => {
          settlements++;
          return { error };
        },
      );
      await nativeWaitFor(() => daemon.requests.length === 2);
      const key = [...sdk.pending.keys()].find((key) => !previous.has(key));
      expect(key).toBeDefined();
      const pending = sdk.pending.get(key!);
      clock.advance(250);
      clock.suspend(5_000);
      clock.resume();
      await nativeWaitFor(() => pending?.sleepGrace?.used === true);
      expect(sdk.pending.get(key!)).toBe(pending);
      expect(settlements).toBe(0);
      clock.advance(9_999);
      expect(settlements).toBe(0);
      expect(
        daemon.respond(1, {
          content: [{ type: "text", text: "running" }],
          isError: false,
          structuredContent: { success: true, task_id: "bash-sdk-wake", status: "running" },
        }),
      ).toBe(true);
      await nativeWaitFor(() => settlements === 1);
      expect(await outcome).toMatchObject({
        value: { success: true, task_id: "bash-sdk-wake", status: "running" },
      });
      expect(sdk.pending.has(key!)).toBe(false);
      expect(daemon.requests).toHaveLength(2);
      expect(daemon.connections).toBe(1);
      expect(daemon.closedRoutes).toEqual([]);
    } finally {
      clears.mockRestore();
      timers.mockRestore();
      awake.mockRestore();
      wall.mockRestore();
    }
  });

  test("pool shutdown (also run on host exit) reports an in-flight bash call as outcome-unknown", async () => {
    const { pool, daemon } = await rig();
    const call = pool
      .getBridge(TEST_PROJECT_ROOT)
      .send("bash", { command: "touch marker", session_id: "shutdown" });
    const settled = call.catch((error: unknown) => error);
    await waitFor(() => daemon.requests.length === 1, "the bash request to reach the daemon");
    expect(daemon.requests[0]?.name).toBe("bash");

    await pool.shutdown();

    expectUnknownOutcomeForBash(await settled, "transport shutting down");
  });

  test("session close reports an in-flight bash call as outcome-unknown", async () => {
    const { pool, daemon } = await rig();
    const settled = pool
      .getBridge(TEST_PROJECT_ROOT)
      .send("bash", { command: "touch marker", session_id: "closing" })
      .catch((error: unknown) => error);
    await waitFor(() => daemon.requests.length === 1, "the bash request to reach the daemon");

    await pool.closeSession(TEST_PROJECT_ROOT, "closing");

    expectUnknownOutcomeForBash(await settled, "session closed");
  });

  test("project-root close reports an in-flight bash call as outcome-unknown and keeps the reap mark", async () => {
    const registry = new LifecycleRegistry({
      timer: { setInterval: () => 0, clearInterval: () => undefined },
      demandCheck: () => true,
      stat: async () => true,
    });
    const { pool, daemon } = await rig({ lifecycle: registry });
    const root = asCanonicalRootPath(TEST_INFLIGHT_ROOT);
    const bridge = pool.getBridge(root);
    const settled = bridge
      .send("bash", { command: "touch marker", session_id: "reaped" })
      .catch((error: unknown) => error);
    await waitFor(() => daemon.requests.length === 1, "the bash request to reach the daemon");

    const poolId = pool.getConcretePoolId();
    const generation = bridge.getGeneration();
    if (poolId === undefined || generation === undefined) throw new Error("no lifecycle ids");
    await registry.requestProjectRootClose(poolId, root, generation, "explicit");

    const error = await settled;
    expectUnknownOutcomeForBash(error, "project root closed");
    expect((error as { subcTeardownReason?: string }).subcTeardownReason).toBe("root_reaped");
  });

  test("cancelling a sibling drains the old route while new calls use its replacement", async () => {
    const { pool, daemon } = await rig();
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    // Open the session's route first so both calls share it.
    daemon.reply = LIVE_REPLY;
    await bridge.send("read", { session_id: "shared" });
    daemon.reply = null;

    const abort = new AbortController();
    const cancelled = bridge
      .send("read", { session_id: "shared" }, { abortSignal: abort.signal })
      .catch((error: unknown) => error);
    const sibling = bridge
      .send("bash", { command: "touch marker", session_id: "shared" })
      .catch((error: unknown) => error);
    await waitFor(() => daemon.requests.length === 3, "both requests to reach the daemon");

    const oldChannel = daemon.requests[1]!.channel;
    expect(daemon.requests[2]!.channel).toBe(oldChannel);
    abort.abort();

    expect(((await cancelled) as Error).name).toBe("AbortError");
    daemon.reply = LIVE_REPLY;
    expect(JSON.stringify(await bridge.send("read", { session_id: "shared" }))).toContain("live");
    expect(daemon.requests[3]!.channel).not.toBe(oldChannel);
    expect(daemon.closedRoutes).not.toContain(oldChannel);
    // Host cancellation does not settle the client's pending unary request.
    expect(daemon.respond(1)).toBe(true);
    expect(daemon.closedRoutes).not.toContain(oldChannel);
    expect(daemon.respond(2)).toBe(true);
    expect(JSON.stringify(await sibling)).toContain("live");
    await waitFor(() => daemon.closedRoutes.includes(oldChannel), "retired route to close");
    expect(daemon.respond(2)).toBe(false);
  });

  test("unanswered calls on a slow live route do not replace it or cut a sibling", async () => {
    const { pool, daemon } = await rig();
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    const sibling = bridge.send("bash", { session_id: "slow" }).catch((error: unknown) => error);
    await waitFor(() => daemon.requests.length === 1, "slow bash request");
    const channel = daemon.requests[0]!.channel;
    for (let attempt = 0; attempt < 3; attempt += 1) {
      const error = await bridge
        .send("read", { session_id: "slow" }, { transportTimeoutMs: 60 })
        .catch((caught: unknown) => caught);
      expect(error).toBeInstanceOf(SubcError);
      expect((error as SubcError).code).toBe("request_deadline");
    }
    await waitFor(() => daemon.catalogProbes === 1, "liveness probe");
    expect(daemon.requests.map((request) => request.channel)).toEqual([
      channel,
      channel,
      channel,
      channel,
    ]);
    expect(daemon.connections).toBe(1);
    expect(daemon.closedRoutes).toEqual([]);
    expect(daemon.respond(0)).toBe(true);
    expect(JSON.stringify(await sibling)).toContain("live");
    daemon.reply = LIVE_REPLY;
    await bridge.send("read", { session_id: "slow" });
    expect(daemon.requests.at(-1)!.channel).toBe(channel);
  });

  test("a reload refusal replaces the route without cutting a pending sibling", async () => {
    const { pool, daemon } = await rig();
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    const sibling = bridge.send("bash", { session_id: "reload" }).catch((error: unknown) => error);
    const refused = bridge.send("read", { session_id: "reload" });
    await waitFor(() => daemon.requests.length === 2, "both reload requests");
    const channel = daemon.requests[0]!.channel;
    daemon.reply = LIVE_REPLY;
    expect(
      daemon.fail(
        1,
        "module_reloading",
        `module endpoint for route channel ${channel} is reloading`,
      ),
    ).toBe(true);
    expect(JSON.stringify(await refused)).toContain("live");
    expect(daemon.requests[2]!.channel).not.toBe(channel);
    expect(daemon.closedRoutes).not.toContain(channel);
    expect(daemon.respond(0)).toBe(true);
    expect(JSON.stringify(await sibling)).toContain("live");
    await waitFor(() => daemon.closedRoutes.includes(channel), "refused route to drain");
  });

  test("a cancelled request pins its retired carrier until its client deadline", async () => {
    const { pool, daemon } = await rig();
    pool.observeSessionStart(TEST_PROJECT_ROOT, "pin");
    const abort = new AbortController();
    const cancelled = pool
      .getBridge(TEST_PROJECT_ROOT)
      .send("read", { session_id: "pin" }, { abortSignal: abort.signal, transportTimeoutMs: 150 })
      .catch((error: unknown) => error);
    await waitFor(() => daemon.requests.length === 1, "pin request");
    const channel = daemon.requests[0]!.channel;
    abort.abort();
    expect(((await cancelled) as Error).name).toBe("AbortError");
    await pool.reapIdleSessions(Date.now() + 16 * 60_000);
    expect(pool.__retainedSessionCountsForTests().sessions).toBe(1);
    expect(daemon.closedRoutes).not.toContain(channel);
    await waitFor(() => daemon.closedRoutes.includes(channel), "cancelled client's deadline");
    expect(pool.__retainedSessionCountsForTests().sessions).toBe(0);
  });

  test("a call on a retired route reports its own deadline, not route replacement", async () => {
    const { pool, daemon } = await rig();
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    const abort = new AbortController();
    const cancelled = bridge
      .send(
        "read",
        { session_id: "expiry" },
        { abortSignal: abort.signal, transportTimeoutMs: 2_000 },
      )
      .catch((error: unknown) => error);
    const sibling = bridge
      .send("bash", { session_id: "expiry" }, { transportTimeoutMs: 150 })
      .catch((error: unknown) => error);
    await waitFor(() => daemon.requests.length === 2, "both expiry requests");
    const channel = daemon.requests[0]!.channel;
    abort.abort();
    expect(((await cancelled) as Error).name).toBe("AbortError");
    const error = await sibling;
    expect(error).toBeInstanceOf(SubcError);
    expect((error as SubcError).code).toBe("request_deadline");
    expect(error).not.toBeInstanceOf(SubcRouteClosedMidCallError);
    expect(daemon.closedRoutes).not.toContain(channel);
    expect(daemon.respond(0)).toBe(true);
    await waitFor(() => daemon.closedRoutes.includes(channel), "retired route deadline cleanup");
  });

  test("connection loss after dispatch carries bash and tool outcome guidance", async () => {
    const { pool, daemon } = await rig();
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    const abort = new AbortController();
    const cancelled = bridge
      .send("read", { session_id: "lost" }, { abortSignal: abort.signal })
      .catch((error: unknown) => error);
    const bash = bridge.send("bash", { session_id: "lost" }).catch((error: unknown) => error);
    const tool = bridge.send("read", { session_id: "lost" }).catch((error: unknown) => error);
    await waitFor(() => daemon.requests.length === 3, "all loss requests to reach the daemon");
    expect(daemon.requests[1]?.name).toBe("bash");
    expect(daemon.requests[2]?.name).toBe("read");

    abort.abort();
    expect(((await cancelled) as Error).name).toBe("AbortError");
    daemon.loseConnections();

    const bashError = await bash;
    expect(bashError).not.toBeInstanceOf(SubcRouteClosedMidCallError);
    expect((bashError as SubcError).closeReason).toBe("connection_lost");
    expect((bashError as { kind?: unknown }).kind).toBe("outcome_unknown");
    const bashAdapted = adaptToolError("bash", bashError) as Error;
    expect(bashAdapted.message).toContain("may still be running");
    expect(bashAdapted.message).toContain("completion notice will follow");
    expect(bashAdapted.message).toContain("Do not rerun it.");
    expect(bashAdapted.message).not.toContain(BASH_TRANSPORT_DISPOSITION);

    const toolError = await tool;
    expect(toolError).not.toBeInstanceOf(SubcRouteClosedMidCallError);
    expect((toolError as SubcError).closeReason).toBe("connection_lost");
    expect((toolError as { kind?: unknown }).kind).toBe("outcome_unknown");
    const toolAdapted = adaptToolError("read", toolError) as Error;
    expect(toolAdapted.message).toContain(SUBC_CONNECTION_CLOSED_MID_CALL_DISPOSITION);
    expect(toolAdapted.message).not.toContain("Nothing was started, so it is safe to retry");

    daemon.reply = LIVE_REPLY;
    expect(JSON.stringify(await bridge.send("read", { session_id: "lost" }))).toContain("live");
    expect(daemon.connections).toBe(2);
  });

  test("explicit session close still cuts calls on a retired route", async () => {
    const { pool, daemon } = await rig();
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    const abort = new AbortController();
    const cancelled = bridge
      .send("read", { session_id: "retired-close" }, { abortSignal: abort.signal })
      .catch((error: unknown) => error);
    const sibling = bridge
      .send("bash", { session_id: "retired-close" })
      .catch((error: unknown) => error);
    await waitFor(() => daemon.requests.length === 2, "both retired-close requests");
    abort.abort();
    await cancelled;
    await pool.closeSession(TEST_PROJECT_ROOT, "retired-close");
    expectUnknownOutcomeForBash(await sibling, "session closed");
  });

  for (const teardown of ["shutdown", "session close"] as const) {
    test(`${teardown} before the request is written stays pre-dispatch, so bash may fall back`, async () => {
      const { pool, daemon } = await rig();
      daemon.holdRouteOpens = true;
      const settled = pool
        .getBridge(TEST_PROJECT_ROOT)
        .send("bash", { command: "touch marker", session_id: "opening" })
        .catch((error: unknown) => error);
      await waitFor(() => daemon.heldRouteOpens === 1, "the route.open to reach the daemon");

      const closing =
        teardown === "shutdown" ? pool.shutdown() : pool.closeSession(TEST_PROJECT_ROOT, "opening");
      daemon.releaseRouteOpens();
      await closing;

      const error = await settled;
      expect(daemon.requests).toHaveLength(0);
      expect(error).not.toBeInstanceOf(SubcRouteClosedMidCallError);
      expect(isSubcRouteTornDownError(error)).toBe(true);
      expect(classifyBashHostFallbackError(error)).toBe("route closed before dispatch");
    });
  }

  test("closing with no request in flight raises nothing, and later calls stay pre-dispatch", async () => {
    const { pool, daemon } = await rig();
    daemon.reply = LIVE_REPLY;
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    await bridge.send("read", { session_id: "idle" });

    // A session reopened after an idle close carries on as normal.
    await pool.closeSession(TEST_PROJECT_ROOT, "idle");
    expect(JSON.stringify(await bridge.send("read", { session_id: "idle" }))).toContain("live");

    await pool.shutdown();
    const error = await bridge
      .send("bash", { command: "true", session_id: "idle" })
      .catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(SubcTransportShuttingDownError);
    expect(classifyBashHostFallbackError(error)).toBe("transport down");
    expect(daemon.requests).toHaveLength(2);
  });
});
