import { AftRpcClient, type AftRpcEndpoint, subscribeRpcRedirects } from "../shared/rpc-client";
import { resolveCortexKitStorageRoot } from "../shared/storage-paths";

export interface SocketNotification {
  id: number;
  type: string;
  payload: Record<string, unknown>;
  sessionId?: string;
}

export interface StatusInvalidation {
  sessionId?: string;
}

export interface TuiSocketOptions {
  getDirectory: () => string | null | undefined;
  getSessionId: () => string | null;
  onNotification: (notification: SocketNotification) => boolean | Promise<boolean>;
}

interface WebSocketLike {
  readyState: number;
  addEventListener(
    type: "open" | "message" | "close" | "error",
    listener: (event: unknown) => void,
  ): void;
  send(data: string): void;
  close(): void;
}

type WebSocketFactory = new (url: string) => WebSocketLike;
type RpcClientLike = Pick<AftRpcClient, "resolveEndpoint" | "reset">;

interface SocketDeps {
  createClient: (directory: string) => RpcClientLike;
  WebSocketCtor: WebSocketFactory | null;
  setTimeout: typeof setTimeout;
  clearTimeout: typeof clearTimeout;
}

interface SocketScope {
  directory: string;
  sessionId: string;
}

const RECONNECT_BASE_MS = 500;
const RECONNECT_MAX_MS = 10_000;
const WEB_SOCKET_OPEN = 1;
const clients = new Map<string, AftRpcClient>();
const statusInvalidationListeners = new Set<(event: StatusInvalidation) => void>();
const lastHandledIdBySession = new Map<string, number>();
const socketEndpoints = new WeakMap<WebSocketLike, AftRpcEndpoint>();
const CURSOR_CACHE_LIMIT = 200;

function cursorKey(ws: WebSocketLike, sessionId: string): string | null {
  const endpoint = socketEndpoints.get(ws);
  // The authentication token distinguishes server instances, even when a new instance reuses the port.
  return endpoint?.token ? `${endpoint.token}\0${sessionId}` : null;
}

function defaultWebSocketCtor(): WebSocketFactory | null {
  const ctor = (globalThis as typeof globalThis & { WebSocket?: WebSocketFactory }).WebSocket;
  return ctor ?? null;
}

function defaultClient(directory: string): AftRpcClient {
  let client = clients.get(directory);
  if (!client) {
    client = new AftRpcClient(resolveCortexKitStorageRoot(), directory);
    clients.set(directory, client);
  }
  return client;
}

let deps: SocketDeps = {
  createClient: defaultClient,
  WebSocketCtor: defaultWebSocketCtor(),
  setTimeout,
  clearTimeout,
};

let opts: TuiSocketOptions | null = null;
let socket: WebSocketLike | null = null;
let socketScope: SocketScope | null = null;
let helloedScope: SocketScope | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
let reconnectAttempt = 0;
let closed = true;
let generation = 0;
let connectingGeneration: number | null = null;
let connectingScope: SocketScope | null = null;

export function startAftTuiSocket(options: TuiSocketOptions): void {
  opts = options;
  closed = false;
  generation += 1;
  ensureRedirectSubscription();
  void reconcileSocketScope();
}

/**
 * Re-home the socket when a status call learns a verified-directory redirect
 * (see rpc-client). The socket may already be connected to this directory's
 * bridgeless instance — which authenticates and accepts hello but will never
 * push a status-changed frame, because the session's bridge lives in another
 * process. Drop it and reconnect; resolveEndpoint now follows the redirect.
 */
let redirectUnsubscribe: (() => void) | null = null;
function ensureRedirectSubscription(): void {
  if (redirectUnsubscribe) return;
  redirectUnsubscribe = subscribeRpcRedirects((from) => {
    if (closed) return;
    const scope = currentScope();
    if (!scope || scope.directory !== from) return;
    generation += 1;
    connectingGeneration = null;
    connectingScope = null;
    closeCurrentSocket(false);
    void connect(scope, generation);
  });
}

export function stopAftTuiSocket(): void {
  closed = true;
  generation += 1;
  connectingGeneration = null;
  connectingScope = null;
  if (redirectUnsubscribe) {
    redirectUnsubscribe();
    redirectUnsubscribe = null;
  }
  if (reconnectTimer) {
    deps.clearTimeout(reconnectTimer);
    reconnectTimer = undefined;
  }
  closeCurrentSocket(false);
  for (const client of clients.values()) client.reset();
  clients.clear();
  lastHandledIdBySession.clear();
  reconnectAttempt = 0;
}

export function refreshAftTuiSocketScope(): void {
  if (closed) return;
  void reconcileSocketScope();
}

export function subscribeStatusInvalidations(
  listener: (event: StatusInvalidation) => void,
): () => void {
  statusInvalidationListeners.add(listener);
  return () => {
    statusInvalidationListeners.delete(listener);
  };
}

export function createDebouncedStatusRefresh(
  refresh: () => void | Promise<void>,
  delayMs: number,
): { schedule: () => void; dispose: () => void } {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;
  return {
    schedule() {
      if (disposed) return;
      if (timer) deps.clearTimeout(timer);
      timer = deps.setTimeout(() => {
        timer = undefined;
        void refresh();
      }, delayMs);
    },
    dispose() {
      disposed = true;
      if (timer) {
        deps.clearTimeout(timer);
        timer = undefined;
      }
    },
  };
}

function currentScope(): SocketScope | null {
  const directory = opts?.getDirectory() ?? "";
  const sessionId = opts?.getSessionId() ?? "";
  if (!directory || !sessionId) return null;
  return { directory, sessionId };
}

function sameScope(a: SocketScope | null, b: SocketScope | null): boolean {
  return a?.directory === b?.directory && a?.sessionId === b?.sessionId;
}

async function reconcileSocketScope(): Promise<void> {
  if (closed || !opts) return;
  const scope = currentScope();
  if (!scope) {
    generation += 1;
    connectingGeneration = null;
    connectingScope = null;
    closeCurrentSocket(false);
    return;
  }

  if (socket && socketScope?.directory === scope.directory) {
    if (!sameScope(socketScope, scope) && socket.readyState !== WEB_SOCKET_OPEN) {
      generation += 1;
      closeCurrentSocket(false);
      await connect(scope, generation);
      return;
    }
    if (socket.readyState === WEB_SOCKET_OPEN && !sameScope(helloedScope, scope)) {
      await sendHelloWithFreshToken(socket, scope, generation);
    }
    return;
  }

  if (sameScope(connectingScope, scope)) return;

  generation += 1;
  connectingGeneration = null;
  connectingScope = null;
  closeCurrentSocket(false);
  await connect(scope, generation);
}

async function connect(scope: SocketScope, connectGeneration: number): Promise<void> {
  if (closed || socket || connectingGeneration !== null) return;
  const WebSocketCtor = deps.WebSocketCtor ?? defaultWebSocketCtor();
  if (!WebSocketCtor) {
    scheduleReconnect();
    return;
  }

  connectingGeneration = connectGeneration;
  connectingScope = scope;
  let endpoint: AftRpcEndpoint | null = null;
  try {
    endpoint = await deps.createClient(scope.directory).resolveEndpoint();
  } catch {
    endpoint = null;
  }

  if (connectingGeneration === connectGeneration) {
    connectingGeneration = null;
    connectingScope = null;
  }
  if (closed || connectGeneration !== generation || !sameScope(currentScope(), scope)) return;
  if (!endpoint) {
    scheduleReconnect();
    return;
  }

  let ws: WebSocketLike;
  try {
    ws = new WebSocketCtor(`ws://127.0.0.1:${endpoint.port}/ws`);
  } catch {
    scheduleReconnect();
    return;
  }

  socket = ws;
  socketEndpoints.set(ws, endpoint);
  socketScope = scope;
  helloedScope = null;

  ws.addEventListener("open", () => {
    if (socket !== ws || connectGeneration !== generation) return;
    reconnectAttempt = 0;
    sendHello(ws, scope, endpoint.token);
  });

  ws.addEventListener("message", (event) => {
    if (socket !== ws || connectGeneration !== generation) return;
    const data =
      typeof (event as { data?: unknown }).data === "string"
        ? (event as { data: string }).data
        : String((event as { data?: unknown }).data ?? "");
    void handleSocketMessage(ws, data, connectGeneration);
  });

  const onDown = () => {
    if (socket !== ws) return;
    socket = null;
    socketScope = null;
    helloedScope = null;
    generation += 1;
    scheduleReconnect();
  };
  ws.addEventListener("close", onDown);
  ws.addEventListener("error", () => {
    try {
      ws.close();
    } catch {
      // best-effort
    }
    onDown();
  });
}

function closeCurrentSocket(schedule: boolean): void {
  const ws = socket;
  socket = null;
  socketScope = null;
  helloedScope = null;
  if (ws) {
    try {
      ws.close();
    } catch {
      // best-effort
    }
  }
  if (schedule) scheduleReconnect();
}

function scheduleReconnect(): void {
  if (closed || reconnectTimer) return;
  const scope = currentScope();
  if (!scope) return;
  const delay = Math.min(RECONNECT_BASE_MS * 2 ** reconnectAttempt, RECONNECT_MAX_MS);
  reconnectAttempt += 1;
  reconnectTimer = deps.setTimeout(() => {
    reconnectTimer = undefined;
    const nextScope = currentScope();
    if (!nextScope) return;
    void connect(nextScope, generation);
  }, delay);
}

async function sendHelloWithFreshToken(
  ws: WebSocketLike,
  scope: SocketScope,
  expectedGeneration: number,
): Promise<void> {
  let endpoint: AftRpcEndpoint | null = null;
  try {
    endpoint = await deps.createClient(scope.directory).resolveEndpoint();
  } catch {
    endpoint = null;
  }
  if (
    closed ||
    socket !== ws ||
    expectedGeneration !== generation ||
    !sameScope(currentScope(), scope)
  ) {
    return;
  }
  const connected = socketEndpoints.get(ws);
  if (!endpoint || endpoint.port !== connected?.port || endpoint.token !== connected.token) {
    generation += 1;
    closeCurrentSocket(false);
    void connect(scope, generation);
    return;
  }
  sendHello(ws, scope, connected.token);
}

function sendHello(ws: WebSocketLike, scope: SocketScope, token: string | null): void {
  helloedScope = scope;
  if (socket === ws) socketScope = scope;
  const key = cursorKey(ws, scope.sessionId);
  const lastReceivedId = key ? (lastHandledIdBySession.get(key) ?? 0) : 0;
  ws.send(
    JSON.stringify({
      type: "hello",
      token: token ?? "",
      sessionId: scope.sessionId,
      lastReceivedId,
    }),
  );
}

async function handleSocketMessage(
  ws: WebSocketLike,
  raw: string,
  messageGeneration: number,
): Promise<void> {
  let msg: {
    type?: string;
    notification?: SocketNotification;
    sessionId?: string;
    error?: string;
  };
  try {
    msg = JSON.parse(raw);
  } catch {
    return;
  }

  if (msg.type === "status-changed") {
    for (const listener of statusInvalidationListeners) {
      try {
        listener({ sessionId: msg.sessionId });
      } catch {
        // One sidebar/dialog listener must not block the others.
      }
    }
    return;
  }

  if (msg.type === "notification" && msg.notification) {
    const notification = msg.notification;
    const active = opts?.getSessionId() ?? null;
    if (!active) return;
    if (notification.sessionId && notification.sessionId !== active) return;

    let consumed = false;
    try {
      consumed = await Promise.resolve(opts?.onNotification(notification) ?? false);
    } catch {
      consumed = false;
    }

    if (
      socket !== ws ||
      messageGeneration !== generation ||
      (opts?.getSessionId() ?? null) !== active
    ) {
      return;
    }
    const key = cursorKey(ws, active);
    if (consumed && notification.id > (key ? (lastHandledIdBySession.get(key) ?? 0) : 0)) {
      if (key) {
        lastHandledIdBySession.delete(key);
        lastHandledIdBySession.set(key, notification.id);
        if (lastHandledIdBySession.size > CURSOR_CACHE_LIMIT) {
          const oldest = lastHandledIdBySession.keys().next().value;
          if (oldest !== undefined) lastHandledIdBySession.delete(oldest);
        }
      }
      try {
        ws.send(JSON.stringify({ type: "ack", lastReceivedId: notification.id }));
      } catch {
        // best-effort; reconnect hello replays the cursor.
      }
    }
    return;
  }

  if (msg.type === "error") {
    try {
      ws.close();
    } catch {
      // best-effort
    }
  }
}

export function __setAftTuiSocketDepsForTest(overrides: Partial<SocketDeps>): () => void {
  const previous = deps;
  deps = { ...deps, ...overrides };
  return () => {
    deps = previous;
  };
}

export function __resetAftTuiSocketForTest(): void {
  stopAftTuiSocket();
  opts = null;
  statusInvalidationListeners.clear();
  lastHandledIdBySession.clear();
  clients.clear();
  reconnectAttempt = 0;
  generation = 0;
  connectingGeneration = null;
  connectingScope = null;
  closed = true;
  deps = {
    createClient: defaultClient,
    WebSocketCtor: defaultWebSocketCtor(),
    setTimeout,
    clearTimeout,
  };
}
