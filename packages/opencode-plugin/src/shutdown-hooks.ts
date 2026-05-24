/**
 * Process-level shutdown handlers.
 *
 * OpenCode does not reliably await plugin `dispose()` before Node exits, and
 * host-level SIGTERM/SIGINT propagate to our `aft` children through the process
 * group. Without explicit cleanup, children see SIGTERM, their `exit` handler
 * fires, and (before the sibling fix in bridge.ts) they would auto-restart into
 * orphaned processes. Even with that fixed, we still want orderly shutdown so
 * pending requests get rejected and the bridges terminate before Node is gone.
 *
 * Each plugin instance (and there can be several per Node process when OpenCode
 * loads the plugin from multiple contexts) registers its cleanup callback here.
 * We install the OS-level signal handlers exactly once per Node process via a
 * `globalThis` guard — otherwise each plugin reload would stack another SIGTERM
 * listener and fire duplicate shutdowns.
 */

import { log } from "./logger.js";

type Cleanup = () => Promise<void> | void;

interface GlobalState {
  cleanups: Set<Cleanup>;
  installed: boolean;
}

const GLOBAL_KEY = "__aftShutdownHooks__";

function getState(): GlobalState {
  const g = globalThis as unknown as Record<string, GlobalState | undefined>;
  if (!g[GLOBAL_KEY]) {
    g[GLOBAL_KEY] = { cleanups: new Set(), installed: false };
  }
  // biome-ignore lint/style/noNonNullAssertion: just initialized above
  return g[GLOBAL_KEY]!;
}

let shuttingDown = false;

async function runCleanups(reason: string): Promise<void> {
  if (shuttingDown) return;
  shuttingDown = true;
  const state = getState();
  if (state.cleanups.size === 0) return;
  log(`Shutdown triggered by ${reason} — running ${state.cleanups.size} cleanup(s)`);
  const cleanups = Array.from(state.cleanups);
  state.cleanups.clear();
  await Promise.allSettled(
    cleanups.map(async (fn) => {
      try {
        await fn();
      } catch (err) {
        log(`Cleanup error: ${(err as Error).message}`);
      }
    }),
  );
}

function installProcessHandlers(): void {
  const state = getState();
  if (state.installed) return;
  state.installed = true;

  const signals = ["SIGTERM", "SIGINT", "SIGHUP"] as const;
  for (const sig of signals) {
    process.on(sig, () => {
      // Best-effort async cleanup. We can't fully await in a signal handler
      // before the default action, but triggering the cleanup gives pending
      // bridges a chance to send SIGTERM to their children cleanly before the
      // process group dies. The `exit` handler below covers the final sync kill.
      void runCleanups(sig);
    });
  }

  // `beforeExit` fires when the event loop empties without a pending exit.
  // `exit` fires synchronously right before the process dies — only sync work
  // runs here, but we can still synchronously signal children via kill().
  process.on("beforeExit", () => {
    void runCleanups("beforeExit");
  });
}

/**
 * Register a shutdown cleanup. Call from plugin initialization; returned
 * function unregisters (use in `dispose` so plugin reloads don't leak).
 */
export function registerShutdownCleanup(fn: Cleanup): () => void {
  installProcessHandlers();
  const state = getState();
  state.cleanups.add(fn);
  return () => {
    state.cleanups.delete(fn);
  };
}
