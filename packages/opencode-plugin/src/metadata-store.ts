/**
 * Pending tool metadata store.
 *
 * OpenCode's `fromPlugin()` wrapper always replaces plugin metadata with
 * `{ truncated, outputPath }`, discarding title and custom metadata.
 *
 * This store captures metadata during execute(), then the `tool.execute.after`
 * hook consumes it and merges it back before the final part is written.
 *
 * Flow:
 *   execute() → storeToolMetadata(sessionID, callID, data)
 *   fromPlugin() → overwrites metadata with { truncated }
 *   tool.execute.after → consumeToolMetadata(sessionID, callID) → merges back
 */

export interface PendingToolMetadata {
  title?: string;
  metadata?: Record<string, unknown>;
}

const pendingStore = new Map<string, PendingToolMetadata & { storedAt: number }>();

const STALE_TIMEOUT_MS = 15 * 60 * 1000;

function makeKey(sessionID: string, callID: string): string {
  return `${sessionID}:${callID}`;
}

function cleanupStaleEntries(): void {
  const now = Date.now();
  for (const [key, entry] of pendingStore) {
    if (now - entry.storedAt > STALE_TIMEOUT_MS) {
      pendingStore.delete(key);
    }
  }
}

/**
 * Store metadata to be restored after fromPlugin() overwrites it.
 * Called from tool execute() functions.
 */
export function storeToolMetadata(
  sessionID: string,
  callID: string,
  data: PendingToolMetadata,
): void {
  cleanupStaleEntries();
  pendingStore.set(makeKey(sessionID, callID), {
    ...data,
    storedAt: Date.now(),
  });
}

/**
 * Consume stored metadata (one-time read, removes from store).
 * Called from tool.execute.after hook.
 */
export function consumeToolMetadata(
  sessionID: string,
  callID: string,
): PendingToolMetadata | undefined {
  const key = makeKey(sessionID, callID);
  const stored = pendingStore.get(key);
  if (stored) {
    pendingStore.delete(key);
    const { storedAt: _, ...data } = stored;
    return data;
  }
  return undefined;
}
