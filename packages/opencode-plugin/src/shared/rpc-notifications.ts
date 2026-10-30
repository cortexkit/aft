/**
 * In-memory notification queue for server → TUI push.
 *
 * The queue is the durable at-least-once buffer; live WebSocket sinks are only
 * a low-latency delivery path. A notification is pruned only after the TUI acks
 * it with the highest id it handled.
 */

export interface RpcNotification {
  id: number;
  type: string;
  payload: Record<string, unknown>;
  sessionId?: string;
}

export interface NotificationSink {
  /** The TUI session that the sink's socket authenticated for. */
  sessionId?: string;
  /** Deliver one queued notification over the live socket. */
  send: (notification: RpcNotification) => void;
}

export interface StatusChangeSink {
  /** The TUI session that the sink's socket authenticated for. */
  sessionId?: string;
  /** Deliver a lightweight status invalidation over the live socket. */
  send: (event: RpcStatusChange) => void;
}

export interface RpcStatusChange {
  sessionId?: string;
}

let queue: RpcNotification[] = [];
let nextNotificationId = 1;
const notificationSinks = new Set<NotificationSink>();
const statusChangeSinks = new Set<StatusChangeSink>();

/** Register a live TUI notification sink. Returns an unregister function. */
export function registerNotificationSink(sink: NotificationSink): () => void {
  notificationSinks.add(sink);
  return () => {
    notificationSinks.delete(sink);
  };
}

/** Register a live TUI status-invalidation sink. Returns an unregister function. */
export function registerStatusChangeSink(sink: StatusChangeSink): () => void {
  statusChangeSinks.add(sink);
  return () => {
    statusChangeSinks.delete(sink);
  };
}

function notificationMatchesSink(
  notification: RpcNotification,
  sink: { sessionId?: string },
): boolean {
  return (
    notification.sessionId === undefined ||
    sink.sessionId === undefined ||
    notification.sessionId === sink.sessionId
  );
}

function statusChangeMatchesSink(event: RpcStatusChange, sink: { sessionId?: string }): boolean {
  return (
    event.sessionId === undefined ||
    sink.sessionId === undefined ||
    event.sessionId === sink.sessionId
  );
}

/** Push a notification for the TUI. Fans out to live sinks and keeps the queue. */
export function pushNotification(
  type: string,
  payload: Record<string, unknown>,
  sessionId?: string,
): void {
  const notification: RpcNotification = { id: nextNotificationId++, type, payload, sessionId };
  queue.push(notification);
  for (const sink of notificationSinks) {
    if (!notificationMatchesSink(notification, sink)) continue;
    try {
      sink.send(notification);
    } catch {
      // A dead socket must not block other sinks; the queue replays on reconnect.
    }
  }
  // Cap queue size to prevent unbounded growth if a TUI is not draining.
  // Session-fair eviction: a naive `slice(-50)` drops the globally-oldest
  // items, so a noisy session could evict ANOTHER session's single unseen
  // notification. Instead, always retain each session's newest item, then
  // fill the rest of the budget with the newest overall — no session can
  // starve another's pending dialog out of the window.
  if (queue.length > 100) {
    const newestPerSession = new Map<string | undefined, number>();
    for (const notification of queue) {
      const previous = newestPerSession.get(notification.sessionId);
      if (previous === undefined || notification.id > previous) {
        newestPerSession.set(notification.sessionId, notification.id);
      }
    }
    const mustKeep = new Set(newestPerSession.values());
    const kept: RpcNotification[] = [];
    // Push assigns monotonic ids and pruning preserves order, so walking backwards is newest-first.
    for (let index = queue.length - 1; index >= 0; index--) {
      const notification = queue[index];
      if (!notification) continue;
      if (kept.length < 50 || mustKeep.has(notification.id)) kept.push(notification);
    }
    queue = kept.reverse();
  }
}

/** Push a lightweight status invalidation to matching live TUI sockets. */
export function pushStatusChange(sessionId?: string): void {
  const event: RpcStatusChange = sessionId ? { sessionId } : {};
  for (const sink of statusChangeSinks) {
    if (!statusChangeMatchesSink(event, sink)) continue;
    try {
      sink.send(event);
    } catch {
      // A dead socket must not block other status listeners.
    }
  }
}

/** Return pending notifications after acking the client's last received id.
 *
 *  Session scoping: when `sessionId` is provided, only notifications tagged for
 *  that session (or session-less/global ones) are returned and pruned — a
 *  notification tagged for a DIFFERENT session is never handed to this client
 *  and is never pruned by this client's ack. This matters because the in-memory
 *  queue is per-process but a TUI can end up draining a process that also serves
 *  OTHER sessions: e.g. opening OpenCode Desktop on the same project starts a
 *  newer RPC server that the TUI's port discovery (newest-pid-wins) then selects,
 *  so a Desktop-session dialog action would otherwise surface in an unrelated
 *  TUI session. Each client also tracks its own `lastReceivedId`, so a global
 *  watermark prune would let session A's ack drop session B's still-unseen
 *  notification — scoping the prune to the acking session prevents that too.
 *
 *  Delivery is at-least-once (non-destructive return + prune-on-ack): a returned
 *  notification stays queued until a later call acks it via a higher
 *  `lastReceivedId`, so a dropped WS socket re-delivers the backlog on reconnect
 *  (the client sends its `lastReceivedId` in the hello). */
export function drainNotifications(lastReceivedId = 0, sessionId?: string): RpcNotification[] {
  const matchesClient = (notification: RpcNotification): boolean =>
    sessionId === undefined ||
    notification.sessionId === undefined ||
    notification.sessionId === sessionId;
  if (lastReceivedId > 0) {
    // Prune only notifications THIS client both owns (session-matched) and has
    // acked (id <= lastReceivedId). Other sessions' notifications survive.
    queue = queue.filter(
      (notification) => !(notification.id <= lastReceivedId && matchesClient(notification)),
    );
  }
  return queue.filter(
    (notification) => notification.id > lastReceivedId && matchesClient(notification),
  );
}

/** Whether a TUI client is connected via a live notification socket. */
export function isTuiConnected(sessionId?: string): boolean {
  if (notificationSinks.size === 0) return false;
  if (sessionId === undefined) return true;
  for (const sink of notificationSinks) {
    if (sink.sessionId === undefined || sink.sessionId === sessionId) return true;
  }
  return false;
}

export function __resetRpcNotificationsForTest(): void {
  queue = [];
  nextNotificationId = 1;
  notificationSinks.clear();
  statusChangeSinks.clear();
}
