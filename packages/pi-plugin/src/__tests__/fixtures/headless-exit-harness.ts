/**
 * Child-process harness for headless-exit.test.ts. Run it with `bun run`; it
 * is not a test file itself.
 *
 * It loads the real Pi extension the way a headless `pi -p` run does, then
 * fires `session_shutdown` while an LSP auto-install is still running, and
 * lets the event loop drain. The parent test measures how long this process
 * lives after the shutdown hook returns.
 *
 * Only the seams that would need a real aft binary or a network download are
 * replaced (binary resolution, storage migration, ONNX Runtime download, and
 * the concrete bridge pool). The pool is still wrapped in the real
 * RevivableTransportPool, so anything that calls into the pool after shutdown
 * really does revive it. A "bridge" in the fake pool is a ref'd interval, the
 * same way a real bridge's child-process pipes keep Node alive, so a bridge
 * spawned after shutdown keeps this process running just like the reported
 * hang.
 *
 * Environment (set by the parent test):
 *   HARNESS_NPM_MARKER  file the fake `npm` writes its pid to once it starts
 *   HARNESS_PLUGIN      absolute path of the plugin entry (src/index.ts)
 *   HARNESS_MODE        "sigterm": skip session_shutdown, add a second SIGTERM
 *                       listener that never exits, and wait for the parent to
 *                       send SIGTERM
 *
 * Stdout protocol, one line each: `EVENT <name> <detail>`.
 */

import { existsSync } from "node:fs";

const realBridge = await import("@cortexkit/aft-bridge");

function emit(name: string, detail = ""): void {
  process.stdout.write(`EVENT ${name} ${detail}\n`);
}

let poolsCreated = 0;
let bridgesSpawned = 0;

function makeFakeInnerPool() {
  poolsCreated += 1;
  const poolId = poolsCreated;
  let shutDown = false;
  const liveBridges = new Map<string, ReturnType<typeof setInterval>>();
  const bridgeFor = (root: string) => ({
    cwd: root,
    async send(command: string) {
      if (!liveBridges.has(root)) {
        bridgesSpawned += 1;
        emit("bridge-spawn", `pool=${poolId} command=${command}`);
        // Stands in for the bridge child's stdio pipes: ref'd until shutdown.
        liveBridges.set(
          root,
          setInterval(() => undefined, 1_000),
        );
      }
      return { success: true };
    },
    async toolCall() {
      return { text: "", success: true };
    },
    cacheStatusSnapshot() {},
    getCachedStatus() {
      return null;
    },
    getCwd() {
      return root;
    },
  });
  return {
    getBridge: (root: string) => bridgeFor(root),
    getActiveBridgeForRoot: (root: string) => (liveBridges.has(root) ? bridgeFor(root) : null),
    activeBridges: () => [],
    setConfigureOverride() {},
    async reconfigure() {},
    async replaceBinary(path: string) {
      return path;
    },
    async closeSession() {},
    async toolCall() {
      return { text: "", success: true };
    },
    async shutdown() {
      shutDown = true;
      for (const timer of liveBridges.values()) clearInterval(timer);
      liveBridges.clear();
    },
    isShutdown: () => shutDown,
  };
}

let resolveOnnx: (dir: string | null) => void = () => undefined;
const onnxReady = new Promise<string | null>((resolve) => {
  resolveOnnx = resolve;
});

Bun.plugin({
  name: "headless-exit-bridge-seams",
  setup(build) {
    build.module("@cortexkit/aft-bridge", () => ({
      loader: "object",
      exports: {
        ...realBridge,
        findBinarySync: () => "/fake/aft",
        findBinary: async () => "/fake/aft",
        ensureBinary: async () => "/fake/aft",
        ensureStorageMigrated: async () => undefined,
        // Resolves only after shutdown, so the eager warmup is still waiting
        // on it when the host tears the session down.
        ensureOnnxRuntime: () => onnxReady,
        createAftTransportPool: async () =>
          new realBridge.RevivableTransportPool(makeFakeInnerPool() as never, async () => {
            emit("pool-revived");
            return makeFakeInnerPool() as never;
          }),
      },
    }));
  },
});

const pluginPath = process.env.HARNESS_PLUGIN;
const npmMarker = process.env.HARNESS_NPM_MARKER;
if (!pluginPath || !npmMarker) throw new Error("harness env missing");

const handlers = new Map<string, Array<(...args: unknown[]) => unknown>>();
const pi = {
  registerTool() {},
  registerCommand() {},
  on(event: string, handler: (...args: unknown[]) => unknown) {
    const list = handlers.get(event) ?? [];
    list.push(handler);
    handlers.set(event, list);
  },
};

const plugin = (await import(pluginPath)).default as (api: unknown) => Promise<void>;
await plugin(pi);
emit("plugin-ready");

// Shut down only once the LSP install has really spawned its npm child.
const deadline = Date.now() + 15_000;
while (!existsSync(npmMarker) && Date.now() < deadline) {
  await new Promise((resolve) => setTimeout(resolve, 25));
}
emit(existsSync(npmMarker) ? "npm-started" : "npm-never-started");

if (process.env.HARNESS_MODE === "sigterm") {
  // Stand in for a host that also listens for SIGTERM but never exits from it
  // (Pi's signal-exit listener stands aside while another listener exists),
  // and that keeps running on its own, like an interactive host would.
  process.on("SIGTERM", function hostListenerThatNeverExits() {
    emit("host-listener-called");
  });
  setInterval(() => undefined, 1_000);
  emit("awaiting-signal");
} else {
  for (const handler of handlers.get("session_shutdown") ?? []) {
    await handler({}, {});
  }
  // The ONNX Runtime becomes ready only now, after the pool is gone.
  resolveOnnx("/fake/onnxruntime");
  emit("shutdown-done", `pools=${poolsCreated} bridges=${bridgesSpawned}`);
}
