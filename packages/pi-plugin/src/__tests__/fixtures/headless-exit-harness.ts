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
 * Every mode except "plugin-validate" fires `session_start` right after the
 * factory returns, as Pi and OMP do as soon as a session exists.
 *
 * Environment (set by the parent test):
 *   HARNESS_NPM_MARKER  file the fake `npm` writes its pid to once it starts
 *   HARNESS_PLUGIN      absolute path of the plugin entry (src/index.ts)
 *   HARNESS_MODE        "sigterm": skip session_shutdown, add a second SIGTERM
 *                       listener that never exits, and wait for the parent to
 *                       send SIGTERM; "subagent": expect MAGIC_CONTEXT_PI_SUBAGENT=1,
 *                       watch startup stay quiet, then call aft_outline;
 *                       "plugin-validate": load the extension the way OMP's
 *                       `plugin install` / `plugin upgrade` validates it (call
 *                       the factory, fire no session event at all), with the
 *                       ONNX Runtime ready at once, then let the script end;
 *                       "session-warmup": ONNX Runtime ready at once, fire
 *                       session_start, wait for the warmup bridge, shut down;
 *                       "session-repeat": like "session-warmup", but the host
 *                       also fires before_agent_start and a second
 *                       session_start (a new session in the same process)
 *                       before shutting down
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
  // Bridges spawn lazily on first use, like the real pool's.
  const spawnIfNeeded = (root: string, reason: string) => {
    if (liveBridges.has(root)) return;
    bridgesSpawned += 1;
    emit("bridge-spawn", `pool=${poolId} ${reason}`);
    // Stands in for the bridge child's stdio pipes: ref'd until shutdown.
    liveBridges.set(
      root,
      setInterval(() => undefined, 1_000),
    );
  };
  const bridgeFor = (root: string) => ({
    cwd: root,
    async send(command: string) {
      emit("bridge-send", `command=${command}`);
      spawnIfNeeded(root, `command=${command}`);
      return { success: true };
    },
    async toolCall(_sessionId: string | undefined, name: string) {
      spawnIfNeeded(root, `tool=${name}`);
      emit("bridge-tool-call", name);
      return { text: "outline ok", success: true };
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

const mode = process.env.HARNESS_MODE;
let resolveOnnx: (dir: string | null) => void = () => undefined;
const onnxReady = new Promise<string | null>((resolve) => {
  resolveOnnx = resolve;
});
// The ONNX Runtime is available at once (as with a system install), so in
// these modes nothing but the plugin's own startup decisions can hold back a
// warmup.
if (mode === "plugin-validate" || mode === "session-warmup" || mode === "session-repeat") {
  resolveOnnx("/fake/onnxruntime");
}

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
        // Outside plugin-validate mode this resolves only after shutdown, so
        // the eager warmup is still waiting on it when the host tears the
        // session down.
        ensureOnnxRuntime: () => {
          emit("onnx-prepare");
          return onnxReady;
        },
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
const tools = new Map<string, { execute: (...args: unknown[]) => Promise<unknown> }>();
const pi = {
  registerTool(tool: { name: string; execute: (...args: unknown[]) => Promise<unknown> }) {
    tools.set(tool.name, tool);
  },
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

const sessionCtx = (sessionId: string) => ({
  cwd: process.cwd(),
  hasUI: false,
  sessionManager: { getSessionId: () => sessionId },
});

if (mode === "plugin-validate") {
  // OMP's PluginManager.install() calls loadExtensions() on the installed entry
  // and returns; the CLI then prints "Installed ..." and its main() returns.
  // There is no session, so no session_start and no session_shutdown. The
  // parent measures how long this process lives after the next line.
  emit("validate-done", `bridges=${bridgesSpawned}`);
} else {
  emit("session-start-fired");
  for (const handler of handlers.get("session_start") ?? []) {
    await handler({ type: "session_start", reason: "startup" }, sessionCtx("harness-session"));
  }
  emit("session-started");
}

if (mode === "plugin-validate") {
  // Nothing more: the script ends here, like the OMP CLI command does.
} else if (mode === "session-warmup") {
  // A real session still gets its warmup bridge at session start, before any
  // tool call, so deferring it out of the factory costs no first-call latency.
  const deadline = Date.now() + 5_000;
  while (bridgesSpawned === 0 && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  emit("warmup-observed", `bridges=${bridgesSpawned}`);
  for (const handler of handlers.get("session_shutdown") ?? []) {
    await handler({}, {});
  }
  emit("shutdown-done", `pools=${poolsCreated} bridges=${bridgesSpawned}`);
} else if (mode === "session-repeat") {
  // Every later session signal must find the startup work already begun:
  // one ONNX Runtime preparation and one warmup status call in total.
  const deadline = Date.now() + 5_000;
  while (bridgesSpawned === 0 && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  emit("warmup-observed", `bridges=${bridgesSpawned}`);
  emit("before-agent-start-fired");
  for (const handler of handlers.get("before_agent_start") ?? []) {
    await handler(
      { type: "before_agent_start", prompt: "hello", systemPrompt: "base" },
      sessionCtx("harness-session"),
    );
  }
  emit("second-session-start-fired");
  for (const handler of handlers.get("session_start") ?? []) {
    await handler({ type: "session_start", reason: "new" }, sessionCtx("harness-session-2"));
  }
  // Long enough for a second warmup `status` call, had startup run again, to
  // reach the fake bridge pool and show up as a second bridge-send event.
  await new Promise((resolve) => setTimeout(resolve, 500));
  for (const handler of handlers.get("session_shutdown") ?? []) {
    await handler({}, {});
  }
  emit("shutdown-done", `pools=${poolsCreated} bridges=${bridgesSpawned}`);
} else if (mode === "subagent") {
  // Give any eager startup work time to show itself: an npm spawn, an ONNX
  // Runtime preparation, or a warmup bridge would all land well within this.
  await new Promise((resolve) => setTimeout(resolve, 1_500));
  emit(existsSync(npmMarker) ? "npm-started" : "npm-never-started");
  emit("startup-quiet", `bridges=${bridgesSpawned}`);
  const outline = tools.get("aft_outline");
  if (!outline) throw new Error("aft_outline was not registered");
  const extCtx = sessionCtx("subagent-session");
  const result = await outline.execute("call-1", { target: "a.ts" }, undefined, undefined, extCtx);
  emit("tool-result", JSON.stringify(result).slice(0, 200));
  for (const handler of handlers.get("session_shutdown") ?? []) {
    await handler({}, {});
  }
  emit("shutdown-done", `pools=${poolsCreated} bridges=${bridgesSpawned}`);
} else {
  // Shut down only once the LSP install has really spawned its npm child.
  const deadline = Date.now() + 15_000;
  while (!existsSync(npmMarker) && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  emit(existsSync(npmMarker) ? "npm-started" : "npm-never-started");

  if (mode === "sigterm") {
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
}
