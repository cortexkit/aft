import { Effect } from "effect";

type MetadataContext = { metadata?: (update: Record<string, unknown>) => unknown };

/** Serialize live UI updates: OpenCode 1 may return a lazy Effect, while OpenCode 2 returns void. */
export function metadataUpdater(context: MetadataContext) {
  let pending = Promise.resolve();
  return (metadata: Record<string, unknown>): Promise<void> => {
    pending = pending
      .then(async () => {
        const result = context.metadata?.({ metadata });
        if (Effect.isEffect(result))
          await Effect.runPromise(result as Effect.Effect<unknown, unknown>);
        else await result;
      })
      // A failed live UI update must not turn successful command output into a tool
      // failure, and later updates must still be allowed to run.
      .catch(() => {});
    return pending;
  };
}
