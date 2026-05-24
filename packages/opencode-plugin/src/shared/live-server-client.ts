/**
 * Workaround helper for the OpenCode plugin promptAsync runner-split bug
 * (https://github.com/anomalyco/opencode/issues/28202).
 *
 * OpenCode's plugin-provided `input.client` is constructed with
 * `fetch: async (...args) => Server.Default().app.fetch(...args)`, which
 * routes requests through `HttpApiApp.webHandler()` and a SEPARATE Effect
 * `memoMap` from the one used by the live HTTP listener. Since
 * `SessionRunState` is a per-memo-map in-memory layer, plugin-origin
 * `promptAsync` calls observe an "idle" runner while the live UI turn is
 * still running. The result is that `ensureRunning` fails to coalesce and
 * OpenCode persists multiple assistant children under a single synthetic
 * user parent — what users see as duplicate "stop" messages after every
 * background-bash completion reminder.
 *
 * The workaround is to bypass `input.client` for the wake path and build
 * a separate `createOpencodeClient` configured to hit `input.serverUrl`
 * via `globalThis.fetch`. That client enters the same live listener the
 * UI uses, so the active session's `SessionRunState` is the one that
 * resolves `ensureRunning` and overlapping turns coalesce correctly.
 *
 * The workaround only works when the live HTTP listener is actually
 * reachable. OpenCode Desktop (Electron+Node) and TUI launched with
 * `opencode --port 0` bind a real API listener; plain TUI binds an
 * internal-only listener that 404s for `/session/*`. We probe once at
 * plugin init and cache the result. When the listener is unreachable
 * the wake path silently uses the in-process `input.client.session.promptAsync`,
 * which keeps wakes flowing (at the cost of the upstream duplicate-runner
 * bug) instead of producing no notification at all or nagging the user
 * to relaunch with a different flag.
 *
 * Tracked upstream as anomalyco/opencode#28202. When OpenCode fixes the
 * runtime split, this helper and its single consumer in `bg-notifications.ts`
 * can be deleted and the wake path can go back to `input.client`.
 */

import { createOpencodeClient } from "@opencode-ai/sdk";

export type LiveServerClient = ReturnType<typeof createOpencodeClient>;

/**
 * Cache key is `${serverUrl}|${directory}`. Both are stable per OpenCode
 * session/project pair, so one client is reused across many wakes. We don't
 * key on `serverUrl + auth header` because the auth env vars are server-wide
 * — if they change we'd want a fresh client anyway; in practice they're set
 * once at process start.
 */
const clientCache = new Map<string, LiveServerClient>();

function cacheKey(serverUrl: string, directory: string): string {
  return `${serverUrl}|${directory}`;
}

/**
 * Build the Basic-auth header OpenCode's server expects when
 * `OPENCODE_SERVER_PASSWORD` is set. Read at call time (not at module load)
 * so test setup can mutate `process.env` between cases.
 */
function serverAuthHeaders(): Record<string, string> | undefined {
  const password = process.env.OPENCODE_SERVER_PASSWORD;
  if (!password) return undefined;
  const username = process.env.OPENCODE_SERVER_USERNAME ?? "opencode";
  return {
    Authorization: `Basic ${Buffer.from(`${username}:${password}`).toString("base64")}`,
  };
}

/**
 * Return a cached `createOpencodeClient` pointed at the live HTTP listener
 * for the given `(serverUrl, directory)` pair. One client object is reused
 * across many wakes for a given session.
 *
 * The `fetch` is bound to `globalThis.fetch` explicitly. Without this, the
 * SDK would fall back to `globalThis.fetch` anyway in normal Node runtimes,
 * but we set it on purpose so anyone reading this code (or grepping for the
 * bug fix) can see that we intentionally chose the live HTTP transport.
 */
export function getLiveServerClient(serverUrl: string, directory: string): LiveServerClient {
  const key = cacheKey(serverUrl, directory);
  const cached = clientCache.get(key);
  if (cached) return cached;
  const client = createOpencodeClient({
    baseUrl: serverUrl,
    directory,
    headers: serverAuthHeaders(),
    fetch: globalThis.fetch,
  });
  clientCache.set(key, client);
  return client;
}

/** Test helper — drop the cache between cases so each test starts clean. */
export function __resetLiveServerClientCacheForTests(): void {
  clientCache.clear();
}

/**
 * Probe whether `serverUrl` accepts a connection within `timeoutMs`.
 * Returns `true` for any HTTP response (including 4xx / 5xx) since the
 * goal is to confirm the listener exists. Returns `false` on connection
 * refused, DNS failure, timeout, or undefined URL.
 *
 * Used at plugin init to decide whether bg-notifications should use the
 * live-server wake transport (workaround for anomalyco/opencode#28202)
 * or fall back to the in-process `input.client.session.promptAsync`
 * path. Plain TUI (no `--port 0`) binds an internal-only listener that
 * 404s for `/session/...`, so this returns false there; OpenCode
 * Desktop, `opencode run`, and `opencode --port 0` TUI return true.
 */
export async function probeServerReachable(
  serverUrl: string | undefined,
  timeoutMs = 1500,
): Promise<boolean> {
  if (!serverUrl) return false;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    // Hit a path that actually exists on the OpenCode HTTP API so a
    // 200 confirms the API server is up, not just any random listener
    // (e.g. an internal IPC port that happens to accept TCP but rejects
    // all paths with 404 — which is exactly what TUI binds without
    // `--port 0`).
    const probeUrl = new URL("/session", serverUrl).toString();
    const res = await globalThis.fetch(probeUrl, {
      method: "GET",
      signal: controller.signal,
    });
    return res.status >= 200 && res.status < 500;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Per-plugin-process decision: should bg-notifications use the live-server
 * wake transport (workaround for anomalyco/opencode#28202), or fall back
 * to the in-process `input.client.session.promptAsync` path?
 *
 * Set once at plugin init from the result of `probeServerReachable()`.
 * Defaults to `false` if no decision has been recorded yet — that's the
 * safe direction because `input.client.session.promptAsync` is always
 * available (it's part of the plugin contract), whereas the live-server
 * path needs both a probe-confirmed listener and the workaround code
 * itself to be live.
 *
 * Read at wake time (not cached on a closure) so background probes that
 * complete after plugin init still take effect on the next wake.
 */
let liveServerWakeAvailable = false;

/**
 * Record the probe result. Idempotent; if you record twice, the latest
 * value wins. The wake path reads through `useLiveServerWake()`.
 */
export function setLiveServerWakeAvailable(available: boolean): void {
  liveServerWakeAvailable = available;
}

/**
 * Read the cached probe decision. `true` means the wake path should use
 * `getLiveServerClient(serverUrl, directory)` and POST through the live
 * HTTP listener. `false` means fall back to the in-process client passed
 * via plugin context (`input.client`).
 */
export function useLiveServerWake(): boolean {
  return liveServerWakeAvailable;
}

/** Test helper — reset the decision cache between cases. */
export function __resetLiveServerWakeForTests(): void {
  liveServerWakeAvailable = false;
}
