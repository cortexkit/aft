import { acquireBridge, releaseBridge } from "@cortexkit/aft-bridge";
import { Effect } from "effect";

import {
  applyToolSurfaceOverrides,
  createProjectAcceptance,
  createSharedPoolOptions,
  defaultBridgeBootstrapDependencies,
  prepareBridgeEnvironment,
  reportHashlineDowngrade,
  resolveBootstrapConfig,
  unknownDisabledToolsReporter,
} from "../bridge-bootstrap.js";
import { resolveBridgePoolTransportOptions } from "../config.js";
import { buildConfigErrorToolMap } from "../config-error-surface.js";
import { debug, log, warn } from "../logger.js";
import { resolvePluginVersion } from "../plugin-version.js";
import { registerAftConfigErrorRpc, registerAftRpc } from "../rpc/register.js";
import { hoistedV2ToolConsumers } from "../tools/hoisted/v2.js";
import { createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import {
  buildAftToolDefinitions,
  openCodeHashlineEffective,
  registerAftTools,
} from "../tool-registration.js";

// The bridge environment (binary, storage migration, configure overrides, ONNX
// Runtime, LSP installs) comes from the bootstrap shared with the OpenCode 1
// entry; every one of those steps can be replaced here through overrides.
const defaults = {
  ...defaultBridgeBootstrapDependencies,
  buildToolMap: buildAftToolDefinitions,
  registerTools: registerAftTools,
  registerRpc: registerAftRpc,
  registerConfigErrorRpc: registerAftConfigErrorRpc,
  toolConsumers: (context) => ({
    ...hoistedV2ToolConsumers(context),
    ...createV2RuntimeConsumer(context),
  }),
  acquireBridge,
  releaseBridge,
  resolvePoolOptions: resolveBridgePoolTransportOptions,
  resolveVersion: () => resolvePluginVersion(import.meta.url),
};

async function bootLocation(context, location, dependencies) {
  const directory = location.directory;
  // The V2 host has no session UI to deliver startup warnings into, so they
  // go to the plugin log.
  const notify = (message) => warn(message);
  const bootstrap = await resolveBootstrapConfig(directory, notify, dependencies);
  if (!bootstrap.ok) {
    // The config error state: register the tool surface with every call
    // failing, and acquire no bridge.
    log(`AFT is in the config error state for ${directory}; every tool call will fail`);
    return {
      configError: bootstrap.message,
      consumers: {},
      pool: null,
      tools: buildConfigErrorToolMap(
        bootstrap.config,
        bootstrap.message,
        context,
        dependencies.buildToolMap,
      ),
    };
  }
  const config = bootstrap.config;

  const pluginVersion = dependencies.resolveVersion();
  const environment = await prepareBridgeEnvironment(
    { configRoot: directory, lspDirectory: directory, config, pluginVersion, notify },
    dependencies,
  );
  const canonicalDirectory = location.project?.canonical ?? directory;
  const consumers = dependencies.toolConsumers(context);
  // Only a rejected configuration keeps AFT out of a project; there is no
  // config switch that turns it off.
  const isProjectEnabled = createProjectAcceptance(directory, dependencies);
  // getPool is only called on a version mismatch, after the pool exists.
  const pool = await dependencies.acquireBridge(canonicalDirectory, {
    harness: "opencode",
    binaryPath: environment.binaryPath,
    poolOptions: {
      ...dependencies.resolvePoolOptions(config),
      ...createSharedPoolOptions({
        pluginVersion,
        getPool: () => pool,
        isProjectEnabled,
        notifyForRoot: (_projectRoot, message) => notify(message),
        dependencies,
      }),
      ...consumers.bridgeOptions,
    },
    configOverrides: environment.configOverrides,
    subcConnectionFile: config.subc?.connection_file,
  });
  environment.attach(pool);
  const toolContext = {
    pool,
    client: context,
    config,
    hashlineEffective: openCodeHashlineEffective(config),
    storageDir: environment.storageDir,
    isProjectEnabled,
  };
  const tools = dependencies.buildToolMap(
    toolContext,
    config,
    unknownDisabledToolsReporter(notify),
  );
  const registeredTools = new Set(Object.keys(tools));
  const { hashlineEditRegistered } = applyToolSurfaceOverrides(pool, config, registeredTools);
  reportHashlineDowngrade(config, registeredTools, notify);
  toolContext.hashlineEffective = hashlineEditRegistered;
  return { consumers, pool, tools };
}

/**
 * Locations whose runtime is running in this process, by directory, so the
 * start line can say when the host starts a second Location for a directory
 * that already has one.
 */
const runningByDirectory = new Map();
let startsInProcess = 0;

/**
 * The start line names the entry, the Location and how many starts this
 * process has seen. The V2 host calls the server effect once per Location and
 * again when it reloads one, so two start lines on one host launch are two
 * Locations or a reload; with the Location and the matching "stopped" line in
 * the log, a reader can tell which instead of seeing the same line twice.
 */
function logRuntimeStart(location) {
  startsInProcess += 1;
  const directory = location.directory;
  const running = runningByDirectory.get(directory) ?? 0;
  runningByDirectory.set(directory, running + 1);
  const id = typeof location.id === "string" ? location.id : "(no id)";
  const overlap =
    running > 0
      ? `; ${running} other Location(s) for this directory still running`
      : "";
  log(
    `AFT V2 runtime starting (server entry, Location ${id} at ${directory}; start ${startsInProcess} in this process${overlap})`,
  );
  return () => {
    const left = (runningByDirectory.get(directory) ?? 1) - 1;
    if (left > 0) runningByDirectory.set(directory, left);
    else runningByDirectory.delete(directory);
    log(`AFT V2 runtime stopped (server entry, Location ${id} at ${directory})`);
  };
}

export function makeServerEffect(overrides = {}) {
  const dependencies = { ...defaults, ...overrides };

  return (context) => {
    // The V2 host already resolved the Location before invoking the plugin. Capture
    // it now so every registration and transport route belongs to that exact scope.
    const location = context.location;
    // A V1 host's bundled core loader can decode {id, effect} and call this
    // function with a PluginHost context that has no location. Throws inside the
    // boot body are discarded by Effect.ignoreCause, so skip when that
    // capability is missing rather than relying on the throw.
    if (typeof location?.directory !== "string") {
      debug("V2 effect skipped: host context has no location");
      return Effect.void;
    }

    return Effect.gen(function* () {
      // Logged when the program runs, so every start pairs with the stop its
      // finalizer logs when the host disposes the Location.
      const logRuntimeStop = logRuntimeStart(location);
      yield* Effect.addFinalizer(() => Effect.sync(logRuntimeStop));
      const runtime = yield* Effect.promise(() => bootLocation(context, location, dependencies));
      if (!runtime) return;

      if (runtime.configError !== undefined) {
        const rpc = yield* dependencies.registerConfigErrorRpc(context, runtime.configError);
        yield* Effect.addFinalizer(() => Effect.promise(() => rpc.dispose()));
        yield* dependencies.registerTools(context, location, runtime.tools, runtime.consumers);
        return;
      }

      yield* Effect.addFinalizer(() =>
        Effect.promise(async () => {
          runtime.consumers.dispose?.();
          await dependencies.releaseBridge(runtime.pool);
        }),
      );
      const rpc = yield* dependencies.registerRpc(context, location, runtime.pool);
      yield* Effect.addFinalizer(() => Effect.promise(() => rpc.dispose()));
      yield* dependencies.registerTools(
        context,
        location,
        runtime.tools,
        runtime.consumers,
      );
    });
  };
}

export const serverEffect = makeServerEffect();
