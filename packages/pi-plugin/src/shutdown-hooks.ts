/**
 * Process-level shutdown handlers.
 *
 * Pi's `session_shutdown` event fires on normal session-end paths, but not when
 * the host Node process is killed by SIGTERM/SIGINT/SIGHUP. Without explicit
 * cleanup, OS propagates the signal to our `aft` children through the process
 * group, the bridge's `exit` handler fires, and (before the sibling fix in
 * bridge.ts) it would auto-restart into orphaned processes.
 *
 * This is a mirror of packages/opencode-plugin/src/shutdown-hooks.ts. The
 * `globalThis` guard ensures OS-level signal handlers are installed exactly
 * once per Node process, even if the Pi extension loads multiple times.
 */

import { log } from "./logger.js";

type Cleanup = () => Promise<void> | void;

interface GlobalState {
  cleanups: Set<Cleanup>;
  installed: boolean;
}

const GLOBAL_KEY = "__aftPiShutdownHooks__";

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

/** Conventional exit codes for fatal signals (128 + signal number). */
export const SIGNAL_EXIT_CODES = { SIGINT: 130, SIGTERM: 143, SIGHUP: 129 } as const;

/**
 * Longest a signalled process stays alive for cleanup, AFT's and any other
 * listener's, before AFT exits it with the conventional code.
 */
export const SIGNAL_EXIT_BOUND_MS = 5_000;

let signalShutdownStarted = false;

/**
 * Registering ANY listener for SIGINT/SIGTERM/SIGHUP disables Node/Bun's
 * default terminate-on-signal behaviour, so after a signal this handler must
 * make sure the process really ends; a plugin must never leave its host
 * unkillable. Other listeners cannot be trusted to do the exit: Pi's own
 * listener (the signal-exit package) only re-raises when it is the sole
 * listener, so with AFT's handler present it stood aside while AFT deferred to
 * it and SIGTERM did nothing at all.
 *
 * - AFT's listener is the only one: run cleanup, then exit (at most the bound).
 * - Other listeners exist: run cleanup alongside them and give them until the
 *   bound to finish and exit on their own terms. The bound timer is unref'd so
 *   it never keeps an otherwise finished process alive; if the process is
 *   still running when it fires, exit with 128 + signal.
 */
function installProcessHandlers(): void {
  const state = getState();
  if (state.installed) return;
  state.installed = true;

  const signals = ["SIGTERM", "SIGINT", "SIGHUP"] as const;
  for (const sig of signals) {
    const handler = () => {
      const code = SIGNAL_EXIT_CODES[sig];
      const others = process.listenerCount(sig) - 1;
      if (signalShutdownStarted) {
        // A repeated signal while only AFT is cleaning up means "now". With
        // other listeners, the already-armed bound still ends the process.
        if (others === 0) process.exit(code);
        return;
      }
      signalShutdownStarted = true;
      // Record the conventional code now so a process that simply drains
      // after cleanup still reports the signal rather than 0. A host that
      // exits with its own code overrides this.
      process.exitCode = code;
      const exit = () => process.exit(code);
      if (others === 0) {
        // Deliberately ref'd: we own termination, so staying alive for at
        // most the bound while cleanup runs is the point.
        const bound = setTimeout(exit, SIGNAL_EXIT_BOUND_MS);
        void runCleanups(sig).finally(() => {
          clearTimeout(bound);
          exit();
        });
        return;
      }
      const names = process
        .listeners(sig)
        .filter((fn) => fn !== handler)
        .map((fn) => fn.name || fn.toString().slice(0, 80).replace(/\s+/g, " "))
        .join(" | ");
      log(
        `${sig}: running cleanup; ${others} other listener(s) have ${SIGNAL_EXIT_BOUND_MS}ms to exit before AFT exits with ${code}. Others: ${names}`,
      );
      void runCleanups(sig);
      const bound = setTimeout(() => {
        log(`${sig}: process still running ${SIGNAL_EXIT_BOUND_MS}ms after the signal; exiting`);
        exit();
      }, SIGNAL_EXIT_BOUND_MS);
      (bound as { unref?: () => void }).unref?.();
    };
    process.on(sig, handler);
  }
  process.on("beforeExit", () => {
    void runCleanups("beforeExit");
  });
}

/** Register a shutdown cleanup. Returns an unregister function. */
export function registerShutdownCleanup(fn: Cleanup): () => void {
  installProcessHandlers();
  const state = getState();
  state.cleanups.add(fn);
  return () => {
    state.cleanups.delete(fn);
  };
}

export function __shutdownCleanupCountForTests(): number {
  return getState().cleanups.size;
}

export function __resetShutdownCleanupsForTests(): void {
  getState().cleanups.clear();
  shuttingDown = false;
}
