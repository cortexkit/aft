/**
 * Workaround helper for OpenCode plugin wake delivery bug
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
 * Workaround is to bypass `input.client` for wake path and build separate
 * `createOpencodeClient` configured to hit `input.serverUrl` via
 * `globalThis.fetch`. That client enters same live listener UI uses, so
 * active session's `SessionRunState` is one that resolves `ensureRunning`
 * and overlapping turns coalesce correctly. Live wake also uses sync
 * `session.prompt(...)`, and resolution there is delivery proof that
 * OpenCode listener accepted prompt work.
 *
 * The workaround only works when the live HTTP listener is actually
 * reachable. OpenCode Desktop (Electron+Node) and TUI launched with
 * `opencode --port 0` bind a real API listener; plain TUI binds an
 * internal-only listener that 404s for `/session/*`. We probe once at
 * plugin init and cache the result by `serverUrl`. When that server is
 * unreachable, wake path must fall back to plugin-provided in-process
 * client. That fallback keeps wakes flowing, but only live listener path
 * gives sync delivery proof; degraded `promptAsync` fallback may still
 * acknowledge before OpenCode persists or handles prompt.
 *
 * Tracked upstream as anomalyco/opencode#28202. When OpenCode fixes the
 * runtime split, this helper and its single consumer in `bg-notifications.ts`
 * can be deleted and the wake path can go back to `input.client`.
 */

import { createOpencodeClient } from "@opencode-ai/sdk";

export type LiveServerClient = ReturnType<typeof createOpencodeClient>;
type RequestHeaders = Record<string, string>;

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

function normalizeServerUrl(serverUrl: string): string {
  try {
    return new URL(serverUrl).toString();
  } catch {
    return serverUrl;
  }
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

function mergeHeaders(
  base: RequestHeaders | undefined,
  extra: RequestHeaders | undefined,
): RequestHeaders | undefined {
  if (!base && !extra) return undefined;
  return {
    ...(base ?? {}),
    ...(extra ?? {}),
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
export function getLiveServerClient(
  serverUrl: string,
  directory: string,
  headers?: RequestHeaders,
): LiveServerClient {
  if (headers && Object.keys(headers).length > 0) {
    return createOpencodeClient({
      baseUrl: serverUrl,
      directory,
      headers: mergeHeaders(serverAuthHeaders(), headers),
      fetch: globalThis.fetch,
    });
  }
  const key = cacheKey(serverUrl, directory);
  const cached = clientCache.get(key);
  if (cached) return cached;
  const client = createOpencodeClient({
    baseUrl: serverUrl,
    directory,
    headers: mergeHeaders(serverAuthHeaders(), headers),
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
 * Per-server decision: should bg-notifications use the live-server wake
 * transport (workaround for anomalyco/opencode#28202), or fall back to the
 * in-process `input.client.session.promptAsync` path?
 *
 * Keying by `serverUrl` matters because one plugin process can host
 * multiple OpenCode windows with different live listener URLs. A missing
 * keyed decision defaults to `false` for safety: the in-process client is
 * part of the plugin contract, while the live-server path requires a
 * probe-confirmed listener.
 *
 * `legacyLiveServerWakeAvailable` is retained only for older callers/tests
 * that do not pass a `serverUrl`; keyed wake callers never read it.
 */
const liveServerWakeAvailableByServerUrl = new Map<string, boolean>();
let legacyLiveServerWakeAvailable = false;

/**
 * Probe whether `serverUrl` serves OpenCode's HTTP API within `timeoutMs`.
 * Returns `true` only when `/session` proves AFT can actually use API: only
 * `res.ok` counts as reachable. 401/403 are rejected unless env-derived
 * Authorization header makes response OK, because AFT cannot satisfy host
 * `ServerAuth` by any other path. Returns `false` for 401/403/404 (plain
 * TUI's internal listener), 5xx, connection refused, DNS failure, timeout,
 * malformed URL, or undefined URL.
 *
 * The probe records its result in the per-`serverUrl` wake-availability
 * cache. That keeps multiple OpenCode windows with different live listener
 * URLs from sharing one process-global transport decision.
 */
export async function probeServerReachable(
  serverUrl: string | undefined,
  timeoutMs = 1500,
): Promise<boolean> {
  if (!serverUrl) return false;
  const normalizedServerUrl = normalizeServerUrl(serverUrl);
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  let reachable = false;
  try {
    // Hit a path that actually exists on the OpenCode HTTP API so a
    // successful response confirms the API server is up, not just any
    // random listener (e.g. an internal IPC port that accepts TCP but
    // rejects all paths with 404 — exactly what TUI binds without
    // `--port 0`).
    const probeUrl = new URL("/session", serverUrl).toString();
    const res = await globalThis.fetch(probeUrl, {
      method: "GET",
      headers: serverAuthHeaders(),
      signal: controller.signal,
    });
    reachable = res.ok;
  } catch {
    reachable = false;
  } finally {
    clearTimeout(timer);
    liveServerWakeAvailableByServerUrl.set(normalizedServerUrl, reachable);
  }
  return reachable;
}

/**
 * Record a probe result. Prefer the `(serverUrl, available)` form; the
 * single-boolean form is a compatibility fallback for old call sites and
 * tests that do not yet have a URL to key by.
 */
export function setLiveServerWakeAvailable(available: boolean): void;
export function setLiveServerWakeAvailable(serverUrl: string | undefined, available: boolean): void;
export function setLiveServerWakeAvailable(
  serverUrlOrAvailable: string | boolean | undefined,
  available?: boolean,
): void {
  if (typeof serverUrlOrAvailable === "boolean") {
    legacyLiveServerWakeAvailable = serverUrlOrAvailable;
    return;
  }
  if (!serverUrlOrAvailable) {
    legacyLiveServerWakeAvailable = available ?? false;
    return;
  }
  liveServerWakeAvailableByServerUrl.set(
    normalizeServerUrl(serverUrlOrAvailable),
    available ?? false,
  );
}

/**
 * Read the cached probe decision for `serverUrl`. `true` means the wake path
 * should use `getLiveServerClient(serverUrl, directory)` and wake through
 * live HTTP listener with sync `session.prompt(...)`. `false` means fall
 * back to in-process client passed via plugin context (`input.client`).
 */
export function useLiveServerWake(serverUrl?: string, enabled = true): boolean {
  if (!enabled) return false;
  if (!serverUrl) return legacyLiveServerWakeAvailable;
  return liveServerWakeAvailableByServerUrl.get(normalizeServerUrl(serverUrl)) ?? false;
}

/** Test helper — reset the decision cache between cases. */
export function __resetLiveServerWakeForTests(): void {
  liveServerWakeAvailableByServerUrl.clear();
  legacyLiveServerWakeAvailable = false;
}
