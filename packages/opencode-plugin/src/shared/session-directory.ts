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
import { sessionWarn } from "../logger.js";

interface SessionInfo {
  directory?: string;
}

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
  /** Resolved directory, or `null` if lookup failed and we should not retry. */
  directory: string | null;
  /** Wall-clock timestamp of the cache entry, used only for LRU eviction. */
  recordedAt: number;
}

const CACHE_MAX_ENTRIES = 200;
const cache = new Map<string, CacheEntry>();

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
  if (cached) {
    // Refresh LRU position
    cache.delete(sessionId);
    cache.set(sessionId, cached);
    return cached.directory;
  }

  const c = client as OpenCodeClientShape;
  const sessionApi = c?.session;
  if (!sessionApi || typeof sessionApi.get !== "function") {
    setCache(sessionId, null);
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
    // Don't poison the cache on transient errors — but do log once.
    sessionWarn(
      sessionId,
      `[aft-plugin] session.get lookup failed: ${err instanceof Error ? err.message : String(err)}`,
    );
    return null;
  }

  setCache(sessionId, dir);
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
  if (cache.has(sessionId)) return;
  void getSessionDirectory(client, sessionId, fallbackDirectory);
}

/** Test-only: clear the cache between unit tests. */
export function _resetSessionDirectoryCacheForTest(): void {
  cache.clear();
}
