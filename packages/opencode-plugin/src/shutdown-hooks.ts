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

let runningCleanups = false;

export async function runCleanups(reason: string): Promise<void> {
  if (runningCleanups) return;
  runningCleanups = true;
  try {
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
  } finally {
    runningCleanups = false;
  }
}

/** Conventional exit codes for fatal signals (128 + signal number). */
export const SIGNAL_EXIT_CODES = { SIGINT: 130, SIGTERM: 143, SIGHUP: 129 } as const;

/** Cap on cleanup time once WE own process termination for a signal. */
const SIGNAL_CLEANUP_TIMEOUT_MS = 5_000;

/**
 * Decide whether AFT's handler must terminate the process after cleanup.
 *
 * Registering ANY listener for SIGINT/SIGTERM/SIGHUP disables Node/Bun's
 * default terminate-on-signal behavior. If AFT's listener is the only one,
 * the default was suppressed solely by us, so we must exit or the host hangs
 * forever (OpenCode serve + Desktop /event SSE hung on Ctrl-C until SIGKILL).
 * If the host or another plugin also listens, terminating is THEIR call —
 * forcing an exit here would race a host's graceful shutdown.
 */
export function shouldForceExit(otherListenerCount: number): boolean {
  return otherListenerCount === 0;
}

let signalShutdownStarted = false;

function installProcessHandlers(): void {
  const state = getState();
  if (state.installed) return;
  state.installed = true;

  const signals = ["SIGTERM", "SIGINT", "SIGHUP"] as const;
  for (const sig of signals) {
    const handler = () => {
      // Count listeners other than ours at SIGNAL time (the host may have
      // registered after plugin load). See shouldForceExit.
      const others = process.listenerCount(sig) - 1;
      if (!shouldForceExit(others)) {
        // Host owns termination; run best-effort cleanup alongside it. The log
        // line names the deferral so a hang is attributable to the OTHER
        // listener (this exact triage cost a cross-team debugging round).
        const names = process
          .listeners(sig)
          .filter((fn) => fn !== handler)
          .map((fn) => fn.name || fn.toString().slice(0, 80).replace(/\s+/g, " "))
          .join(" | ");
        log(
          `${sig}: deferring termination to ${others} other listener(s); cleanup only. Others: ${names}`,
        );
        void runCleanups(sig);
        return;
      }
      if (signalShutdownStarted) {
        // Second signal while cleanup is in flight: exit immediately.
        process.exit(SIGNAL_EXIT_CODES[sig]);
      }
      signalShutdownStarted = true;
      // Record the intended code immediately so even an unexpected early exit
      // (event loop drains mid-cleanup) reports the signal, not 0.
      process.exitCode = SIGNAL_EXIT_CODES[sig];
      // Deliberately ref'd: we own termination here, so keeping the process
      // alive for at most the cap is the point — an unref'd timer would let
      // the loop drain and exit before the race settles.
      const timeout = new Promise<void>((resolve) => {
        setTimeout(resolve, SIGNAL_CLEANUP_TIMEOUT_MS);
      });
      void Promise.race([runCleanups(sig), timeout]).finally(() => {
        process.exit(SIGNAL_EXIT_CODES[sig]);
      });
    };
    process.on(sig, handler);
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
