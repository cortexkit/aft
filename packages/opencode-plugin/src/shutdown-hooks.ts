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

type Cleanup = (reason: string) => Promise<void> | void;
type RegisteredCleanup = (reason: string) => Promise<void>;

export interface ShutdownCleanupRegistration {
  /** Run only this registration's cleanup and remove it from the process-exit registry. */
  dispose(reason: string): Promise<void>;
  /** Remove the cleanup without running it. */
  unregister(): void;
}

interface GlobalState {
  cleanups: Set<RegisteredCleanup>;
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
const cleanupInvocationCounts = new WeakMap<RegisteredCleanup, () => number>();

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
          await fn(reason);
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
 * unkillable (OpenCode serve + Desktop /event SSE once hung on Ctrl-C until
 * SIGKILL). Other listeners cannot be trusted to do the exit: a listener such
 * as the signal-exit package only re-raises when it is the sole listener, so
 * with AFT's handler present it stands aside while AFT waits for it.
 *
 * - AFT's listener is the only one: run cleanup, then exit (at most the bound).
 * - Other listeners exist: run cleanup alongside them and give them until the
 *   bound to finish a graceful shutdown and exit on their own terms. The bound
 *   timer is unref'd so it never keeps an otherwise finished process alive; if
 *   the process is still running when it fires, exit with 128 + signal.
 */
function installProcessHandlers(): void {
  const state = getState();
  if (state.installed) return;
  state.installed = true;

  const signals = ["SIGTERM", "SIGINT", "SIGHUP"] as const;
  for (const sig of signals) {
    const handler = () => {
      const code = SIGNAL_EXIT_CODES[sig];
      // Count listeners other than ours at SIGNAL time (the host may have
      // registered after plugin load).
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
      // Name the other listeners so a slow shutdown is attributable from the
      // log alone.
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

  // `beforeExit` fires when the event loop empties without a pending exit.
  // `exit` fires synchronously right before the process dies — only sync work
  // runs here, but we can still synchronously signal children via kill().
  process.on("beforeExit", () => {
    void runCleanups("beforeExit");
  });
}

/**
 * Register one plugin instance's cleanup for process-exit drains. The returned
 * registration also owns instance disposal, so removing, running, and
 * de-duplicating that cleanup cannot drift apart at factory call sites.
 */
export function registerShutdownCleanup(fn: Cleanup): ShutdownCleanupRegistration {
  installProcessHandlers();
  const state = getState();
  let cleanupPromise: Promise<void> | null = null;
  let disposePromise: Promise<void> | null = null;
  let invocationCount = 0;

  const registeredCleanup: RegisteredCleanup = async (reason) => {
    invocationCount += 1;
    cleanupPromise ??= Promise.resolve()
      .then(() => fn(reason))
      .catch((err) => {
        log(`Cleanup error: ${(err as Error).message}`);
      });
    await cleanupPromise;
  };
  cleanupInvocationCounts.set(registeredCleanup, () => invocationCount);
  state.cleanups.add(registeredCleanup);

  return {
    dispose(reason) {
      if (disposePromise) return disposePromise;
      state.cleanups.delete(registeredCleanup);
      disposePromise = registeredCleanup(reason);
      return disposePromise;
    },
    unregister() {
      state.cleanups.delete(registeredCleanup);
    },
  };
}

/** Read the registered callback invocation count without exposing it in production state. */
export function __shutdownCleanupInvocationCountForTests(cleanup: unknown): number {
  return typeof cleanup === "function"
    ? (cleanupInvocationCounts.get(cleanup as RegisteredCleanup)?.() ?? 0)
    : 0;
}
