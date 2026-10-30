/**
 * Workaround for OpenCode's session-directory bug: when a user runs
 * `opencode -s <sessionID>` (or otherwise resumes a session) from a
 * directory other than the session's original project directory,
 * OpenCode's tool registry sets `ctx.directory = process.cwd()` for
 * every tool call instead of the session's stored directory.
 *
 * That breaks every plugin that does workspace-scoped work — including
 * AFT, where it caused configure to spin up against the user's home
 * directory and time out trying to index hundreds of thousands of
 * unrelated files.
 *
 * The session itself stores the correct directory in OpenCode's SQLite
 * (`Session.directory` in the SDK). This helper looks it up once per
 * session and caches the result — sessions don't change directory
 * across their lifetime, so the cache never needs invalidation.
 *
 * The bug should also be fixed upstream in OpenCode's
 * `packages/opencode/src/session/registry.ts`. Until then this
 * workaround makes AFT robust against the wrong cwd.
 */

import type { AftTransportPool } from "@cortexkit/aft-bridge";
import { Effect } from "effect";

import { sessionWarn } from "../logger.js";

interface SessionInfo {
  directory?: string;
}

/**
 * OpenCode 2 passes its plugin context where OpenCode 1 passes the SDK client
 * (`entry/server-runtime.mjs` hands the context over as `client`). That context
 * has a different session API, verified against the `v2` branch of the OpenCode repository:
 *
 * - `session.get` takes `{ sessionID }` and returns an Effect of `Session.Info`
 *   (packages/client/src/effect/api/api.ts `SessionGetOperation`,
 *   packages/plugin/src/effect/plugin.ts `Context`). Calling it the OpenCode 1
 *   way builds an Effect that never runs, and awaiting an Effect yields the
 *   Effect itself, so no directory was ever read.
 * - `Session.Info` has no `directory`; the session's working directory is
 *   `location.directory`, a required `Location.Ref`
 *   (packages/schema/src/session.ts `Info`, packages/schema/src/location.ts
 *   `Ref`). For a session created in a linked git worktree it is that worktree.
 *
 * It is told apart from an OpenCode 1 SDK client by having a string
 * `location.directory` next to a `session.get` function; the OpenCode 2 entry
 * refuses to boot without that `location`, and `subagent-detect.ts` uses the
 * same test for its own session lookup.
 */
interface V2SessionInfo {
  location?: { directory?: unknown };
}

interface V2PluginContextShape {
  location: { directory: string };
  session: {
    get(input: { sessionID: string }): Effect.Effect<V2SessionInfo | undefined, unknown>;
  };
}

function isV2PluginContext(client: unknown): client is V2PluginContextShape {
  if (!client || typeof client !== "object") return false;
  const candidate = client as { location?: { directory?: unknown }; session?: { get?: unknown } };
  return (
    typeof candidate.location?.directory === "string" &&
    typeof candidate.session?.get === "function"
  );
}

/**
 * How long a session lookup may run on either host version. Tool routing
 * awaits this lookup, so an unresponsive host must not hold the call forever.
 * After this budget the call proceeds with its own directory instead.
 */
export const LOOKUP_TIMEOUT_MS = 1_000;
export const V2_SESSION_LOOKUP_TIMEOUT_MS = LOOKUP_TIMEOUT_MS;
/** A failed lookup is provisional; retry later without querying on every call. */
export const FAILED_LOOKUP_RETRY_MS = 30_000;

interface OpenCodeClientShape {
  session?: {
    get?: (input: {
      path: { id: string };
      query?: { directory?: string };
      throwOnError?: boolean;
    }) => Promise<{ data?: SessionInfo } | SessionInfo | undefined>;
  };
}

interface CacheEntry {
  /** Resolved directory, or `null` while a failed lookup is cooling down. */
  directory: string | null;
  /** When the answer was recorded; failed lookups become retryable after 30 seconds. */
  recordedAt: number;
}

const CACHE_MAX_ENTRIES = 200;
const cache = new Map<string, CacheEntry>();
const inflight = new Map<string, Promise<string | null>>();
const lookupGenerations = new Map<string, object>();

/**
 * Resolve the project directory the session was created with from the SDK's
 * session object. Returns the SDK-reported directory, or `null` when the
 * lookup is unavailable/fails — callers should fall back to the runtime's
 * directory in that case.
 *
 * This function is best-effort: any error (missing `client.session.get`,
 * network failure, malformed response) is logged and recorded as a
 * negative cache entry so we don't retry on every tool call within the
 * same session.
 */
export async function getSessionDirectory(
  client: unknown,
  sessionId: string,
  fallbackDirectory: string,
): Promise<string | null> {
  if (!sessionId) return null;

  const cached = cache.get(sessionId);
  if (
    cached &&
    (cached.directory !== null || Date.now() - cached.recordedAt < FAILED_LOOKUP_RETRY_MS)
  ) {
    // Refresh LRU position
    cache.delete(sessionId);
    cache.set(sessionId, cached);
    return cached.directory;
  }

  const pending = inflight.get(sessionId);
  if (pending) return pending;
  const lookup = boundedLookup(client, sessionId, fallbackDirectory).finally(() => {
    inflight.delete(sessionId);
  });
  inflight.set(sessionId, lookup);
  return lookup;
}

/** A late success may repair its provisional answer, but never overwrite a newer lookup. */
async function boundedLookup(
  client: unknown,
  sessionId: string,
  fallbackDirectory: string,
): Promise<string | null> {
  const generation = {};
  lookupGenerations.set(sessionId, generation);
  if (lookupGenerations.size > CACHE_MAX_ENTRIES) {
    const oldest = lookupGenerations.keys().next().value;
    if (oldest !== undefined) lookupGenerations.delete(oldest);
  }
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const timeout = new Promise<null>((resolve) => {
      timer = setTimeout(() => {
        sessionWarn(
          sessionId,
          `[aft-plugin] session lookup did not answer within ${LOOKUP_TIMEOUT_MS}ms; using the call's directory for now`,
        );
        resolve(null);
      }, LOOKUP_TIMEOUT_MS);
    });
    const directory = await Promise.race([
      lookupSessionDirectory(client, sessionId, fallbackDirectory)
        .then((directory) => {
          if (lookupGenerations.get(sessionId) === generation && directory !== null) {
            setCache(sessionId, directory);
          }
          return directory;
        })
        .finally(() => {
          if (lookupGenerations.get(sessionId) === generation) lookupGenerations.delete(sessionId);
        }),
      timeout,
    ]);
    setCache(sessionId, directory);
    return directory;
  } finally {
    clearTimeout(timer);
  }
}

async function lookupSessionDirectory(
  client: unknown,
  sessionId: string,
  fallbackDirectory: string,
): Promise<string | null> {
  if (isV2PluginContext(client)) return lookupV2SessionDirectory(client, sessionId);

  const c = client as OpenCodeClientShape;
  const sessionApi = c?.session;
  if (!sessionApi || typeof sessionApi.get !== "function") {
    return null;
  }

  let dir: string | null = null;
  try {
    // Call as a method so the SDK's `this._client` reference resolves
    // correctly. Extracting `c.session.get` into a local would lose the
    // binding and crash the SDK with "undefined is not an object".
    // SDK schema: SessionGetData uses `path: { id }`, NOT a flat
    // `sessionID`. Do NOT pass `directory` — looking up a session by ID is an
    // identity query, and newer SDKs don't accept a top-level directory here.
    void fallbackDirectory;
    const result = await sessionApi.get({ path: { id: sessionId } });
    // SDK responses come either as `{ data: Session }` or directly as `Session`
    // depending on `ThrowOnError`. Handle both shapes.
    const session: SessionInfo | undefined =
      (result as { data?: SessionInfo } | undefined)?.data ?? (result as SessionInfo | undefined);
    if (session && typeof session.directory === "string" && session.directory.length > 0) {
      dir = session.directory;
    }
  } catch (err) {
    // The bounded caller caches failures briefly so a struggling host is not polled per call.
    sessionWarn(
      sessionId,
      `[aft-plugin] session.get lookup failed: ${err instanceof Error ? err.message : String(err)}`,
    );
    return null;
  }

  return dir;
}

/**
 * OpenCode 2 lookup: run the context's `session.get` Effect under a timeout and
 * read `location.directory`.
 *
 * The outer bounded lookup applies the same retry policy as OpenCode 1.
 * The Effect timeout also interrupts the abandoned V2 lookup; fallback remains
 * the tool call's runtime.directory (the host's Location directory), never
 * the main checkout.
 */
async function lookupV2SessionDirectory(
  context: V2PluginContextShape,
  sessionId: string,
): Promise<string | null> {
  let dir: string | null = null;
  let failure: string | undefined;
  try {
    const session = await Effect.runPromise(
      Effect.timeout(
        context.session.get({ sessionID: sessionId }) as Effect.Effect<V2SessionInfo | undefined>,
        V2_SESSION_LOOKUP_TIMEOUT_MS,
      ),
    );
    const directory = session?.location?.directory;
    if (typeof directory === "string" && directory.length > 0) dir = directory;
    else failure = "the session record has no location.directory";
  } catch (err) {
    failure = err instanceof Error ? err.message || err.name : String(err);
  }
  if (failure !== undefined) {
    sessionWarn(
      sessionId,
      `[aft-plugin] OpenCode 2 session lookup failed (${failure}); tool paths and bash use the Location directory ${context.location.directory} for this session`,
    );
  }
  return dir;
}

function setCache(sessionId: string, directory: string | null): void {
  if (cache.has(sessionId)) cache.delete(sessionId);
  cache.set(sessionId, { directory, recordedAt: Date.now() });
  if (cache.size > CACHE_MAX_ENTRIES) {
    const oldest = cache.keys().next().value;
    if (oldest !== undefined) cache.delete(oldest);
  }
}

/**
 * Synchronous cache probe. Returns the resolved directory for a session if we
 * already looked it up; otherwise `undefined` so the caller falls through to
 * its synchronous fallback (typically `runtime.directory`).
 *
 * This is the hot path: `bridgeFor()` runs on every tool call and must not
 * block on an SDK round-trip. The async {@link warmSessionDirectory} should
 * be called eagerly (without await) at the start of each tool call to keep
 * the cache filled, so by the time a second call from the same session
 * arrives, this probe returns the correct directory.
 */
export function getSessionDirectoryCached(
  sessionId: string | undefined,
): string | null | undefined {
  if (!sessionId) return undefined;
  const cached = cache.get(sessionId);
  if (!cached) return undefined;
  if (cached.directory === null && Date.now() - cached.recordedAt >= FAILED_LOOKUP_RETRY_MS) {
    cache.delete(sessionId);
    return undefined;
  }
  return cached.directory;
}

/**
 * Fire-and-forget cache warmup. Safe to call from synchronous code; failures
 * are logged but not propagated. Subsequent calls to {@link getSessionDirectoryCached}
 * will return the resolved directory once the lookup completes.
 */
export function warmSessionDirectory(
  client: unknown,
  sessionId: string | undefined,
  fallbackDirectory: string,
): void {
  if (!sessionId) return;
  if (getSessionDirectoryCached(sessionId) !== undefined) return;
  void getSessionDirectory(client, sessionId, fallbackDirectory);
}

/**
 * Serve-time verification for cross-project bridge resolution.
 *
 * The RPC status handler may serve a bridge for a DIFFERENT project root than
 * the requesting instance's own directory (the `opencode -s` resume case).
 * That cross-directory path must never trust a possibly-stale cache entry:
 * a wrong mapping makes the sidebar render another project's data (RPC
 * contamination). This helper re-resolves the session's directory via a
 * fresh SDK lookup and only returns a directory the SDK confirms RIGHT NOW.
 *
 * Results are memoized briefly (VERIFY_TTL_MS) so a 1.5s sidebar poll doesn't
 * hammer the SDK; the TTL is short enough that stale mappings die quickly.
 * Returns `null` when the lookup fails or the SDK reports no directory —
 * callers must treat that as "do not serve cross-project data".
 */
const VERIFY_TTL_MS = 15_000;
const verifyCache = new Map<string, { directory: string | null; verifiedAt: number }>();
const verifyInflight = new Map<string, Promise<string | null>>();

export async function verifySessionDirectory(
  client: unknown,
  sessionId: string,
): Promise<string | null> {
  if (!sessionId) return null;
  const hit = verifyCache.get(sessionId);
  if (hit && Date.now() - hit.verifiedAt < VERIFY_TTL_MS) return hit.directory;
  const pending = verifyInflight.get(sessionId);
  if (pending) return pending;
  const lookup = boundedVerification(client, sessionId).finally(() => {
    verifyInflight.delete(sessionId);
  });
  verifyInflight.set(sessionId, lookup);
  return lookup;
}

async function boundedVerification(client: unknown, sessionId: string): Promise<string | null> {
  const c = client as OpenCodeClientShape;
  const sessionApi = c?.session;
  if (!sessionApi || typeof sessionApi.get !== "function") return null;

  let dir: string | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const result = await Promise.race([
      sessionApi.get({ path: { id: sessionId } }),
      new Promise<never>((_, reject) => {
        timer = setTimeout(
          () => reject(new Error("Session verification timed out")),
          LOOKUP_TIMEOUT_MS,
        );
      }),
    ]);
    const session: SessionInfo | undefined =
      (result as { data?: SessionInfo } | undefined)?.data ?? (result as SessionInfo | undefined);
    if (session && typeof session.directory === "string" && session.directory.length > 0) {
      dir = session.directory;
    }
  } catch {
    // Verification failure = do not serve cross-project data. Do NOT memoize
    // failures: the next poll should retry (transient SDK errors must not
    // stick the sidebar on placeholder for the TTL window).
    return null;
  } finally {
    clearTimeout(timer);
  }

  verifyCache.set(sessionId, { directory: dir, verifiedAt: Date.now() });
  if (verifyCache.size > CACHE_MAX_ENTRIES) {
    const oldest = verifyCache.keys().next().value;
    if (oldest !== undefined) verifyCache.delete(oldest);
  }
  // Keep the long-lived cache coherent with what the SDK just said.
  if (dir !== null) setCache(sessionId, dir);
  return dir;
}

/** Release transport routes on deletion without querying a session the host has already removed. */
export async function closeDeletedSessionRoutes(
  pool: AftTransportPool,
  sessionId: string,
  fallbackDirectory: string,
): Promise<void> {
  // A timeout may have bound a fallback before the stored root was known. Close both routes.
  const roots = new Set(pool.activeBridges().map((bridge) => bridge.getCwd()));
  roots.add(getSessionDirectoryCached(sessionId) ?? fallbackDirectory);
  await Promise.allSettled([...roots].map((root) => pool.closeSession(root, sessionId)));
  cache.delete(sessionId);
  lookupGenerations.delete(sessionId);
  verifyCache.delete(sessionId);
}

/** Test-only: clear the cache between unit tests. */
export function _resetSessionDirectoryCacheForTest(): void {
  cache.clear();
  inflight.clear();
  lookupGenerations.clear();
  verifyCache.clear();
  verifyInflight.clear();
}
