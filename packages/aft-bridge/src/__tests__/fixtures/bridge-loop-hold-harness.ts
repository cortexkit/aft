/**
 * Child-process harness for bridge-event-loop-hold.test.ts. Run it with
 * `bun run` or `node`; it is not a test file itself.
 *
 * Drives one real BinaryBridge against a stub `aft` executable and then holds
 * nothing of its own, so whether this process exits is decided only by what
 * the bridge keeps referenced.
 *
 * Environment (set by the parent test):
 *   HARNESS_BRIDGE  absolute path of the bridge module (src/bridge.ts)
 *   HARNESS_BINARY  path of the stub aft executable
 *   HARNESS_MODE    "idle": one call completes, then the script ends with the
 *                   warm child still running;
 *                   "busy-gap": the first call is still in flight while the
 *                   bridge awaits the host's configure-warnings handler, which
 *                   settles only when the stub pushes a status frame a second
 *                   later; no request timer is armed during that wait.
 *                   The bridge is shut down after the call, so this mode's
 *                   exit does not depend on the idle release;
 *                   "background-task": the call starts a background bash task
 *                   and returns at once; the stub reports the task finished a
 *                   second later, and the script ends without shutting down
 *
 * Stdout protocol, one line each: `EVENT <name> <detail>`.
 */

function emit(name: string, detail = ""): void {
  process.stdout.write(`EVENT ${name} ${detail}\n`);
}

const bridgePath = process.env.HARNESS_BRIDGE;
const binaryPath = process.env.HARNESS_BINARY;
const mode = process.env.HARNESS_MODE;
if (!bridgePath || !binaryPath || !mode) throw new Error("harness env missing");

// Everything after the import runs in a detached async function, not as a
// top-level await: Bun keeps a process alive while its entry module's
// top-level await is pending, which would hide whether the bridge holds the
// loop. A host's extension code is likewise not the entry module's own await.
const { BinaryBridge } = (await import(bridgePath)) as typeof import("../../bridge.js");

async function main(): Promise<void> {
  let bridge: InstanceType<typeof BinaryBridge>;
  bridge = new BinaryBridge(binaryPath as string, process.cwd(), {
    timeoutMs: 20_000,
    maxRestarts: 0,
    onConfigureWarnings: () =>
      new Promise<void>((resolve) => {
        emit("configure-warnings-waiting");
        const unsubscribe = bridge.subscribeStatus(() => {
          unsubscribe();
          emit("status-frame");
          resolve();
        });
      }),
    onBashCompletion: (completion) => {
      emit("bash-completed", `task=${String(completion.task_id)}`);
    },
  });

  const response =
    mode === "background-task"
      ? await bridge.send("bash", { command: "sleep 1", background: true })
      : await bridge.send("status", {});
  emit("call-settled", `success=${String(response.success)} alive=${String(bridge.isAlive())}`);
  if (mode === "busy-gap") {
    await bridge.shutdown();
    emit("shutdown-done");
  }
}

void main().catch((err: unknown) => {
  emit("harness-error", err instanceof Error ? err.message : String(err));
  process.exitCode = 1;
});
