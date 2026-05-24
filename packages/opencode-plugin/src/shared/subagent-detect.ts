/**
 * Subagent detection via OpenCode SDK.
 *
 * AFT bash auto-promotes anything that takes more than ~5s to a background
 * task. For the main agent (Alfonso), promotion is fine — the completion
 * reminder fires back into the conversation and the agent picks it up on
 * its next idle. For SUBAGENTS, promotion is fatal: a subagent waiting for
 * a background-bash completion must end its turn to receive the reminder,
 * which closes the subagent session permanently. The subagent never gets
 * to commit its work.
 *
 * This module detects whether a session is a subagent (has a non-empty
 * `parentID`) so the bash tool can:
 *   1. Refuse `background: true` outright for subagents.
 *   2. Disable auto-promotion: the foreground poll window extends to the
 *      task's full hard-kill timeout instead of the default 5s.
 *
 * Detection is via the OpenCode SDK `client.session.get`. The result is
 * cached per sessionID for the lifetime of the plugin process — subagent
 * identity is sticky (parentID never changes after session creation), so
 * the cache never needs invalidation.
 *
 * Errors are not cached. If the SDK is briefly unavailable on the first
 * call, we retry on the next bash invocation rather than permanently
 * misclassifying. This is cheap because the cache is hit on every
 * subsequent call within the same session.
 *
 * Mirrors the pattern in `session-directory.ts`.
 */
import { sessionLog, sessionWarn } from "../logger.js";

interface SessionInfo {
  parentID?: string;
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
  isSubagent: boolean;
  recordedAt: number;
}

const CACHE_MAX_ENTRIES = 200;
const cache = new Map<string, CacheEntry>();

/**
 * Returns `true` when the given session has a non-empty `parentID`
 * (subagent). Returns `false` for primary sessions, when the SDK is
 * unavailable, or when any error occurs — the false default keeps
 * primary sessions working exactly as before.
 *
 * First call per session: one SDK round-trip. All subsequent calls:
 * O(1) cache lookup.
 */
export async function resolveIsSubagent(
  client: unknown,
  sessionId: string | undefined,
  _fallbackDirectory?: string,
): Promise<boolean> {
  if (!sessionId) {
    sessionLog(undefined, "[subagent-detect] no sessionId provided → primary");
    return false;
  }

  const cached = cache.get(sessionId);
  if (cached) {
    // Refresh LRU position. Don't log the hit — it's pure noise; the
    // downstream call sites already log their effective gate decisions.
    cache.delete(sessionId);
    cache.set(sessionId, cached);
    return cached.isSubagent;
  }

  const c = client as OpenCodeClientShape;
  const sessionApi = c?.session;
  if (!sessionApi || typeof sessionApi.get !== "function") {
    // SDK shape unavailable. Cache as not-subagent so we don't retry
    // every call when the host doesn't expose session.get.
    sessionLog(
      sessionId,
      `[subagent-detect] client.session.get unavailable (client=${typeof client}, session=${typeof sessionApi}, get=${typeof sessionApi?.get}) → caching as primary`,
    );
    setCache(sessionId, false);
    return false;
  }

  sessionLog(
    sessionId,
    `[subagent-detect] cache miss, calling client.session.get(id=${sessionId})`,
  );

  let isSubagent = false;
  let parentIdRaw: unknown;
  try {
    // Call as a method so the SDK's `this._client` reference resolves
    // correctly. Extracting `sessionApi.get` into a local would lose
    // the binding and crash the SDK with "undefined is not an object".
    // SDK schema: SessionGetData uses `path: { id }`, NOT a flat `sessionID`.
    // We do NOT pass `directory` — looking up a session by ID is an identity
    // query, not a directory-scoped one. Passing the wrong shape returned a
    // different session whose `parentID` was undefined, defeating the gate.
    const result = await sessionApi.get({
      path: { id: sessionId },
    });
    // SDK responses come either as `{ data: Session }` or directly as
    // `Session` depending on `ThrowOnError`. Handle both shapes.
    const session: SessionInfo | undefined =
      (result as { data?: SessionInfo } | undefined)?.data ?? (result as SessionInfo | undefined);
    parentIdRaw = session?.parentID;
    isSubagent =
      session !== undefined && typeof session.parentID === "string" && session.parentID.length > 0;
    sessionLog(
      sessionId,
      `[subagent-detect] SDK returned session=${session !== undefined ? "present" : "undefined"}, parentID=${JSON.stringify(parentIdRaw)} → isSubagent=${isSubagent}`,
    );
  } catch (err) {
    // Don't poison the cache on transient errors — but do log once.
    // Return false so primary-session behavior is preserved.
    sessionWarn(
      sessionId,
      `[subagent-detect] SDK lookup failed: ${err instanceof Error ? err.message : String(err)}`,
    );
    return false;
  }

  setCache(sessionId, isSubagent);
  return isSubagent;
}

function setCache(sessionId: string, isSubagent: boolean): void {
  if (cache.has(sessionId)) cache.delete(sessionId);
  cache.set(sessionId, { isSubagent, recordedAt: Date.now() });
  if (cache.size > CACHE_MAX_ENTRIES) {
    const oldest = cache.keys().next().value;
    if (oldest !== undefined) cache.delete(oldest);
  }
}

/** Test-only cache reset. Not exported from the public surface. */
export function _resetSubagentCacheForTest(): void {
  cache.clear();
}
