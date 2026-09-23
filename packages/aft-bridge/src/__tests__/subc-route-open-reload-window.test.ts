/**
 * Route opens that land inside an AFT module reload window, exercised through a
 * REAL `SubcClient` over a real TCP socket against a small in-process fake subc
 * daemon (handshake + envelope framing, no subc-core binary needed).
 *
 * The fake daemon refuses every `route.open` with the retryable
 * `module_reloading` code until a virtual clock reaches 5,000 ms, then accepts.
 * The virtual clock only advances through the injected retry sleeps, so the
 * "5 s reload" costs no real time and the outcome depends only on how long each
 * layer is willing to keep retrying, not on machine speed.
 */

import { afterEach, describe, expect, test } from "bun:test";
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
  SubcClient,
} from "@cortexkit/subc-client";
import { SubcTransportPool } from "../subc-transport.js";
import { TEST_PROJECT_ROOT } from "./subc-test-roots.js";

const RELOAD_WINDOW_MS = 5_000;

interface FakeDaemon {
  connectionFile: string;
  /** Route opens the daemon refused with module_reloading. */
  refusals: number;
  /** Route opens the daemon accepted. */
  accepted: number;
  /** Data-plane requests answered on an accepted route. */
  requests: number;
  close(): Promise<void>;
}

/**
 * Start a fake subc daemon. `now()` is the virtual clock the reload window is
 * measured against; `reply` builds the body answered to every route request;
 * `reloadWindowMs` is how long route.open keeps being refused.
 */
async function startFakeDaemon(
  now: () => number,
  reply: () => unknown,
  reloadWindowMs: number = RELOAD_WINDOW_MS,
): Promise<FakeDaemon> {
  const key = new Uint8Array(32).fill(7);
  const daemonId = new Uint8Array(16).fill(9);
  const sockets = new Set<Socket>();
  const state = { refusals: 0, accepted: 0, requests: 0 };
  let nextChannel = 1;

  const server: Server = createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => undefined);
    let buffered = Buffer.alloc(0);
    // Handshake messages are 4-byte little-endian length + JSON; after the
    // client's auth message the stream switches to envelope frames.
    let phase: "hello" | "auth" | "frames" = "hello";

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
      socket.write(encodeFrame(frame));
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
            // to script route.open answers for the client under test.
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
        if (header.ty !== FrameType.Request) continue;

        if (header.channel === 0) {
          const op = (JSON.parse(body.toString("utf8")) as { op?: string }).op;
          if (op !== "route.open") {
            send(FrameType.Error, 0, 0, header.corr, {
              code: "unsupported",
              message: `fake daemon does not implement ${op}`,
            });
            continue;
          }
          if (now() < reloadWindowMs) {
            state.refusals += 1;
            send(FrameType.Error, 0, 0, header.corr, {
              code: "module_reloading",
              message: "module_id 'aft' is reloading",
            });
            continue;
          }
          state.accepted += 1;
          send(FrameType.Response, 0, 0, header.corr, {
            op: "route.open",
            route_channel: nextChannel++,
            route_epoch: 1,
          });
          continue;
        }

        state.requests += 1;
        send(FrameType.Response, header.channel, header.epoch, header.corr, reply());
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
    get refusals() {
      return state.refusals;
    },
    get accepted() {
      return state.accepted;
    },
    get requests() {
      return state.requests;
    },
    async close() {
      for (const socket of sockets) socket.destroy();
      await new Promise<void>((resolve) => server.close(() => resolve()));
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

describe("route.open across a 5s module reload window (real SubcClient, fake daemon)", () => {
  const cleanups: Array<() => Promise<void> | void> = [];
  afterEach(async () => {
    while (cleanups.length > 0) await cleanups.pop()?.();
  });

  test("subc-client's managed call keeps retrying module_reloading until the module is back", async () => {
    // Pins the upstream fix: older clients also stopped after 6 attempts
    // (0+100+300+700+1500+3100 ms of backoff ≈ 3.1 s), well inside their own 30 s
    // deadline, and failed with "retry budget exhausted".
    let virtualNowMs = 0;
    const daemon = await startFakeDaemon(
      () => virtualNowMs,
      () => ({ ok: true }),
    );
    cleanups.push(() => daemon.close());
    const client = await SubcClient.connect({
      connectionFile: daemon.connectionFile,
      handshakeTimeoutMs: 2_000,
      sleep: async (ms) => {
        virtualNowMs += ms;
      },
    });
    cleanups.push(() => client.close());

    await expect(
      client.call("aft", "ping", undefined, {
        identity: { project_root: TEST_PROJECT_ROOT, harness: "opencode", session: "reload" },
        consumerIdentity: null,
      }),
    ).resolves.toEqual({ ok: true });
    expect(virtualNowMs).toBeGreaterThanOrEqual(RELOAD_WINDOW_MS);
    expect(daemon.refusals).toBeGreaterThanOrEqual(6);
    expect(daemon.accepted).toBe(1);
  });

  test("an AFT tool call through the bridge waits out the reload window and succeeds", async () => {
    // The bridge opens routes with the client's single-shot routeOpen and runs
    // its own reload-window loop (bounded by the call's deadline), so this holds independently of the
    // managed-call retry policy pinned above.
    let virtualNowMs = 0;
    const daemon = await startFakeDaemon(
      () => virtualNowMs,
      () => ({
        content: [{ type: "text", text: "live" }],
        isError: false,
        structuredContent: { id: "r", success: true, text: "live" },
      }),
    );
    cleanups.push(() => daemon.close());
    const pool = new SubcTransportPool({
      connectionFile: daemon.connectionFile,
      harness: "opencode",
      consumerIdentity: null,
      handshakeTimeoutMs: 2_000,
      routeRetrySleep: async (ms) => {
        virtualNowMs += ms;
      },
    });
    cleanups.push(() => pool.shutdown());

    const reply = await pool.getBridge(TEST_PROJECT_ROOT).toolCall("reload", "read", {});
    expect(JSON.stringify(reply)).toContain("live");
    expect(virtualNowMs).toBeGreaterThanOrEqual(RELOAD_WINDOW_MS);
    expect(daemon.refusals).toBeGreaterThanOrEqual(6);
    expect(daemon.accepted).toBe(1);
    expect(daemon.requests).toBe(1);
  });

  // A real restart drains the old module for up to 30 s and then starts the new
  // one, so the refusal window can outlast 30 s.
  const LONG_RELOAD_WINDOW_MS = 35_000;

  async function longReloadPool(): Promise<{
    pool: SubcTransportPool;
    daemon: FakeDaemon;
    clock: { now: number };
  }> {
    const clock = { now: 0 };
    const daemon = await startFakeDaemon(
      () => clock.now,
      () => ({
        content: [{ type: "text", text: "live" }],
        isError: false,
        structuredContent: { id: "r", success: true, text: "live" },
      }),
      LONG_RELOAD_WINDOW_MS,
    );
    cleanups.push(() => daemon.close());
    const pool = new SubcTransportPool({
      connectionFile: daemon.connectionFile,
      harness: "opencode",
      consumerIdentity: null,
      handshakeTimeoutMs: 2_000,
      routeRetrySleep: async (ms) => {
        clock.now += ms;
      },
    });
    cleanups.push(() => pool.shutdown());
    return { pool, daemon, clock };
  }

  test("a 35s reload is waited out when the call's deadline allows it", async () => {
    const { pool, daemon, clock } = await longReloadPool();

    const reply = await pool
      .getBridge(TEST_PROJECT_ROOT)
      .toolCall("long-reload", "read", {}, { timeoutMs: 60_000 });
    expect(JSON.stringify(reply)).toContain("live");
    expect(clock.now).toBeGreaterThanOrEqual(LONG_RELOAD_WINDOW_MS);
    expect(clock.now).toBeLessThan(60_000);
    expect(daemon.accepted).toBe(1);
    expect(daemon.requests).toBe(1);
  });

  test("a 35s reload surfaces the refusal once a shorter call deadline passes", async () => {
    const { pool, daemon, clock } = await longReloadPool();

    let surfaced: unknown;
    try {
      await pool
        .getBridge(TEST_PROJECT_ROOT)
        .toolCall("short-deadline", "read", {}, { timeoutMs: 20_000 });
    } catch (error) {
      surfaced = error;
    }
    // The refusal keeps its retryable code, so callers still classify it as a
    // module outage and take their fallback.
    expect((surfaced as { code?: string }).code).toBe("module_reloading");
    expect((surfaced as Error).message).toContain("within this call's 20s deadline");
    expect(clock.now).toBeLessThan(20_000);
    expect(daemon.accepted).toBe(0);
    expect(daemon.requests).toBe(0);
  });

  test("a call without its own timeout waits out the reload for the client's 30s default", async () => {
    const { pool, daemon, clock } = await longReloadPool();

    let surfaced: unknown;
    try {
      await pool.getBridge(TEST_PROJECT_ROOT).toolCall("default-deadline", "read", {});
    } catch (error) {
      surfaced = error;
    }
    expect((surfaced as { code?: string }).code).toBe("module_reloading");
    expect((surfaced as Error).message).toContain("within this call's 30s deadline");
    expect(clock.now).toBeGreaterThan(15_000);
    expect(clock.now).toBeLessThan(30_000);
    expect(daemon.accepted).toBe(0);
  });
});
