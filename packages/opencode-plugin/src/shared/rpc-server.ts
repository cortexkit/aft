import { randomBytes, timingSafeEqual } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  renameSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { dirname, join } from "node:path";
import { log, warn } from "../logger";
import {
  drainNotifications,
  type NotificationSink,
  registerNotificationSink,
  registerStatusChangeSink,
  type StatusChangeSink,
} from "./rpc-notifications";
import { isPidAlive, parseRpcPortRecord, rpcPortFileDir } from "./rpc-utils";

type RpcHandler = (params: Record<string, unknown>) => Promise<Record<string, unknown>>;

type BunServe = <Data>(options: BunServeOptions<Data>) => BunServer<Data>;

interface BunRuntime {
  serve: BunServe;
}

interface BunServer<Data> {
  port?: number;
  stop(closeActiveConnections?: boolean): void;
  upgrade(req: Request, options: { data: Data }): boolean;
}

interface BunServeOptions<Data> {
  port: number;
  hostname: string;
  fetch(
    req: Request,
    server: BunServer<Data>,
  ): Response | Promise<Response | undefined> | undefined;
  websocket: {
    open(ws: BunServerWebSocket<Data>): void;
    message(ws: BunServerWebSocket<Data>, raw: string | Buffer): void;
    close(ws: BunServerWebSocket<Data>): void;
  };
}

interface BunServerWebSocket<Data> {
  data: Data;
  send(data: string): unknown;
  close(code?: number, reason?: string): void;
}

interface WsData {
  authed: boolean;
  sessionId?: string;
  unregisterNotification?: () => void;
  unregisterStatus?: () => void;
  authTimer?: ReturnType<typeof setTimeout>;
}

const PORT_FILE_HEARTBEAT_MS = 15_000;
const MAX_BODY_BYTES = 1_048_576;
const WS_AUTH_TIMEOUT_MS = 5_000;
const WS_CLOSE_UNAUTHORIZED = 4401;

function bunRuntime(): BunRuntime | undefined {
  return (globalThis as typeof globalThis & { Bun?: BunRuntime }).Bun;
}

function tokensMatch(presented: string, expected: string): boolean {
  const a = Buffer.from(presented, "utf8");
  const b = Buffer.from(expected, "utf8");
  if (a.length !== b.length) return false;
  return timingSafeEqual(a, b);
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

export class AftRpcServer {
  private nodeServer: Server | null = null;
  private bunServer: BunServer<WsData> | null = null;
  private port = 0;
  private token = randomBytes(32).toString("hex");
  private handlers = new Map<string, RpcHandler>();
  private portFilePath: string;
  private portsDir: string;
  private heartbeatTimer: ReturnType<typeof setInterval> | null = null;
  /** Unique per-instance ID — distinguishes our entry from duplicate plugin loads. */
  private instanceId: string;
  private sockets = new Set<BunServerWebSocket<WsData>>();

  constructor(storageDir: string, directory: string) {
    this.portsDir = rpcPortFileDir(storageDir, directory);
    this.instanceId = randomBytes(8).toString("hex");
    this.portFilePath = join(this.portsDir, `${this.instanceId}.json`);
  }

  /** Register an RPC method handler. */
  handle(method: string, handler: RpcHandler): void {
    this.handlers.set(method, handler);
  }

  /** Start the server on a random port, write port to disk. */
  async start(): Promise<number> {
    const bun = bunRuntime();
    if (bun) {
      return this.startBun(bun);
    }
    return this.startNode();
  }

  private async startBun(bun: BunRuntime): Promise<number> {
    const server = bun.serve<WsData>({
      port: 0,
      hostname: "127.0.0.1",
      fetch: (req, srv) => this.handleFetch(req, srv),
      websocket: {
        open: (ws) => {
          ws.data.authTimer = setTimeout(() => {
            if (!ws.data.authed) ws.close(WS_CLOSE_UNAUTHORIZED, "auth timeout");
          }, WS_AUTH_TIMEOUT_MS);
        },
        message: (ws, raw) => this.handleWsMessage(ws, raw),
        close: (ws) => this.closeWs(ws),
      },
    });

    this.bunServer = server;
    this.port = server.port ?? 0;
    // Match startNode's unref: the RPC server is a side channel and must not be
    // the reason a one-shot run refuses to end. Bun added unref() on its server
    // after this code was written, so guard for older runtimes rather than
    // assume it; a runtime without it keeps the previous behaviour.
    (server as { unref?: () => void }).unref?.();
    this.afterServerStarted();
    return this.port;
  }

  private async startNode(): Promise<number> {
    return new Promise((resolve, reject) => {
      const server = createServer((req, res) => this.dispatch(req, res));

      server.on("error", (err) => {
        warn(`RPC server error: ${err.message}`);
        reject(err);
      });

      server.listen(0, "127.0.0.1", () => {
        const addr = server.address();
        if (!addr || typeof addr === "string") {
          reject(new Error("Failed to get server address"));
          return;
        }
        this.port = addr.port;
        this.nodeServer = server;
        this.afterServerStarted();
        resolve(this.port);
      });

      // Don't keep the process alive just for the RPC server
      server.unref();
    });
  }

  private afterServerStarted(): void {
    try {
      this.writePortFile();
      log(`RPC server listening on 127.0.0.1:${this.port}`);
      // Self-heal: the port file is this server's only discoverability record.
      // Anything that deletes it silently orphans the server until host restart.
      // Recreate it if missing.
      this.heartbeatTimer = setInterval(() => this.ensurePortFile(), PORT_FILE_HEARTBEAT_MS);
      this.heartbeatTimer.unref?.();
      // Hygiene: sweep dead port files from other instances while we're here.
      // Server startup is the natural sweep point and already owns this directory.
      this.sweepDeadPortFiles();
    } catch (err) {
      warn(`Failed to write RPC port file: ${err}`);
    }
  }

  private writePortFile(): void {
    const dir = dirname(this.portFilePath);
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    const tmpPath = `${this.portFilePath}.tmp`;
    writeFileSync(
      tmpPath,
      JSON.stringify({
        port: this.port,
        token: this.token,
        pid: process.pid,
        started_at: Date.now(),
      }),
      { encoding: "utf-8", mode: 0o600 },
    );
    renameSync(tmpPath, this.portFilePath);
  }

  /**
   * Remove sibling port files whose owning process is provably dead.
   * Only files with a recorded pid that no longer maps to a live process are
   * removed; pid-less (legacy) files and live entries are left untouched.
   * Never touches our own freshly written file.
   */
  private sweepDeadPortFiles(): void {
    let entries: string[];
    try {
      entries = readdirSync(this.portsDir);
    } catch {
      return;
    }
    for (const entry of entries) {
      if (!entry.endsWith(".json")) continue;
      const filePath = join(this.portsDir, entry);
      if (filePath === this.portFilePath) continue;
      try {
        const record = parseRpcPortRecord(readFileSync(filePath, "utf-8"));
        // Unparsable file: stale tmp/corrupt leftover — remove. Parsable but
        // pid-less (legacy): keep, we cannot prove the owner is dead.
        if (record === null) {
          unlinkSync(filePath);
          continue;
        }
        if (record.pid !== undefined && !isPidAlive(record.pid)) {
          unlinkSync(filePath);
        }
      } catch {
        // Racing another instance's sweep or hitting a permission issue is
        // fine — the file will be retried on the next server start.
      }
    }
  }

  /** Rewrite the port file if it disappeared (wrongful deletion recovery). */
  private ensurePortFile(): void {
    if ((!this.nodeServer && !this.bunServer) || this.port <= 0) return;
    try {
      if (existsSync(this.portFilePath)) return;
      this.writePortFile();
      log(`RPC port file was missing; rewrote ${this.portFilePath}`);
    } catch {
      // best-effort; retried on the next heartbeat
    }
  }

  /** Stop the server and clean up port file. */
  stop(): void {
    if (this.heartbeatTimer) {
      clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = null;
    }
    for (const ws of this.sockets) {
      try {
        this.closeWs(ws);
        ws.close();
      } catch {
        // Ignore close errors during shutdown; remaining sockets are closed independently.
      }
    }
    this.sockets.clear();
    if (this.nodeServer) {
      this.nodeServer.close();
      this.nodeServer = null;
    }
    if (this.bunServer) {
      this.bunServer.stop(true);
      this.bunServer = null;
    }
    try {
      unlinkSync(this.portFilePath);
    } catch {
      // ignore
    }
  }

  private async handleFetch(req: Request, srv: BunServer<WsData>): Promise<Response | undefined> {
    const url = new URL(req.url);

    if (url.pathname === "/ws") {
      const upgraded = srv.upgrade(req, { data: { authed: false } });
      if (upgraded) return undefined;
      return new Response("upgrade failed", { status: 400 });
    }

    if (req.method === "GET" && url.pathname === "/health") {
      return json({ ok: true, pid: process.pid });
    }

    if (req.method !== "POST" || !url.pathname.startsWith("/rpc/")) {
      return new Response("Not Found", { status: 404 });
    }

    const method = url.pathname.slice(5);
    const handler = this.handlers.get(method);
    if (!handler) {
      return json({ error: `Unknown method: ${method}` }, 404);
    }

    const bodyText = await req.text();
    if (bodyText.length > MAX_BODY_BYTES) {
      return new Response("Request too large", { status: 413 });
    }

    let params: Record<string, unknown> = {};
    try {
      if (bodyText.length > 0) {
        params = JSON.parse(bodyText);
      }
    } catch {
      return json({ error: "Invalid JSON" }, 400);
    }

    if (!tokensMatch(typeof params.token === "string" ? params.token : "", this.token)) {
      return json({ error: "Forbidden" }, 403);
    }

    const { token: _token, ...handlerParams } = params;

    try {
      const result = await handler(handlerParams);
      return json(result);
    } catch (err) {
      log(`RPC error: ${method} => ${err}`);
      return json({ error: String(err) }, 500);
    }
  }

  private handleWsMessage(ws: BunServerWebSocket<WsData>, raw: string | Buffer): void {
    let msg: { type?: string; token?: string; sessionId?: string; lastReceivedId?: number };
    try {
      msg = JSON.parse(typeof raw === "string" ? raw : raw.toString("utf8"));
    } catch {
      return;
    }

    if (msg.type === "hello") {
      if (!tokensMatch(typeof msg.token === "string" ? msg.token : "", this.token)) {
        ws.send(JSON.stringify({ type: "error", error: "unauthorized" }));
        ws.close(WS_CLOSE_UNAUTHORIZED, "bad token");
        return;
      }
      if (ws.data.authTimer) {
        clearTimeout(ws.data.authTimer);
        ws.data.authTimer = undefined;
      }

      ws.data.unregisterNotification?.();
      ws.data.unregisterStatus?.();
      ws.data.authed = true;
      ws.data.sessionId =
        typeof msg.sessionId === "string" && msg.sessionId.length > 0 ? msg.sessionId : undefined;

      const lastReceivedId = Number(msg.lastReceivedId ?? 0);
      const backlog = drainNotifications(
        Number.isFinite(lastReceivedId) ? lastReceivedId : 0,
        ws.data.sessionId,
      );

      const notificationSink: NotificationSink = {
        sessionId: ws.data.sessionId,
        send: (notification) => {
          ws.send(JSON.stringify({ type: "notification", notification }));
        },
      };
      const statusSink: StatusChangeSink = {
        sessionId: ws.data.sessionId,
        send: (event) => {
          ws.send(JSON.stringify({ type: "status-changed", ...event }));
        },
      };
      ws.data.unregisterNotification = registerNotificationSink(notificationSink);
      ws.data.unregisterStatus = registerStatusChangeSink(statusSink);
      this.sockets.add(ws);

      for (const notification of backlog) {
        ws.send(JSON.stringify({ type: "notification", notification }));
      }
      ws.send(JSON.stringify({ type: "hello-ack" }));
      return;
    }

    if (msg.type === "ack") {
      const lastReceivedId = Number(msg.lastReceivedId ?? 0);
      if (Number.isFinite(lastReceivedId) && lastReceivedId > 0) {
        drainNotifications(lastReceivedId, ws.data.sessionId);
      }
    }
  }

  private closeWs(ws: BunServerWebSocket<WsData>): void {
    if (ws.data.authTimer) {
      clearTimeout(ws.data.authTimer);
      ws.data.authTimer = undefined;
    }
    ws.data.unregisterNotification?.();
    ws.data.unregisterNotification = undefined;
    ws.data.unregisterStatus?.();
    ws.data.unregisterStatus = undefined;
    this.sockets.delete(ws);
  }

  private dispatch(req: IncomingMessage, res: ServerResponse): void {
    const url = req.url ?? "";

    if (req.method === "GET" && url === "/health") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ ok: true, pid: process.pid }));
      return;
    }

    if (req.method !== "POST" || !url.startsWith("/rpc/")) {
      res.writeHead(404);
      res.end("Not Found");
      return;
    }

    const method = url.slice(5);
    const handler = this.handlers.get(method);
    if (!handler) {
      res.writeHead(404, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ error: `Unknown method: ${method}` }));
      return;
    }

    let body = "";
    req.on("data", (chunk: Buffer) => {
      body += chunk.toString();
      if (body.length > MAX_BODY_BYTES) {
        res.writeHead(413);
        res.end("Request too large");
        req.destroy();
      }
    });

    req.on("end", () => {
      let params: Record<string, unknown> = {};
      try {
        if (body.length > 0) {
          params = JSON.parse(body);
        }
      } catch {
        res.writeHead(400, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ error: "Invalid JSON" }));
        return;
      }

      if (!tokensMatch(typeof params.token === "string" ? params.token : "", this.token)) {
        res.writeHead(403, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ error: "Forbidden" }));
        return;
      }

      const { token: _token, ...handlerParams } = params;

      // Successful RPC calls are not logged to avoid noise; errors are still logged.
      handler(handlerParams)
        .then((result) => {
          res.writeHead(200, { "Content-Type": "application/json" });
          res.end(JSON.stringify(result));
        })
        .catch((err) => {
          log(`RPC error: ${method} => ${err}`);
          res.writeHead(500, { "Content-Type": "application/json" });
          res.end(JSON.stringify({ error: String(err) }));
        });
    });
  }
}
