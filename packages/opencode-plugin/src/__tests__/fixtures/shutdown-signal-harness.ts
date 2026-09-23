/**
 * Child-process harness for shutdown-signal-exit.test.ts; run with `bun run`.
 *
 * Registers an AFT shutdown cleanup, then adds a second SIGTERM listener that
 * never exits (the shape of a host listener such as signal-exit, which stands
 * aside while other listeners exist) and keeps the process busy the way a
 * running host would. The parent sends SIGTERM and times the exit.
 *
 * Stdout protocol, one line each: `EVENT <name>`.
 */

import { registerShutdownCleanup } from "../../shutdown-hooks.js";

function emit(name: string): void {
  process.stdout.write(`EVENT ${name}\n`);
}

registerShutdownCleanup(async () => {
  emit("aft-cleanup-ran");
});
process.on("SIGTERM", function hostListenerThatNeverExits() {
  emit("host-listener-called");
});
setInterval(() => undefined, 1_000);
emit("awaiting-signal");
