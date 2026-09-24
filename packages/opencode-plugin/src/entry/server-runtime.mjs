import { acquireBridge, releaseBridge } from "@cortexkit/aft-bridge";
import { Effect } from "effect";

import {
  applyToolSurfaceOverrides,
  createProjectAcceptance,
  createSharedPoolOptions,
  defaultBridgeBootstrapDependencies,
  loadBootstrapConfig,
  prepareBridgeEnvironment,
  reportHashlineDowngrade,
  unknownDisabledToolsReporter,
} from "../bridge-bootstrap.js";
import { resolveBridgePoolTransportOptions } from "../config.js";
import { debug, log, warn } from "../logger.js";
import { resolvePluginVersion } from "../plugin-version.js";
import { registerAftRpc } from "../rpc/register.js";
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
  const config = loadBootstrapConfig(directory, notify, dependencies);
  if (!config) return undefined;

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

    log("AFT V2 runtime starting");

    return Effect.gen(function* () {
      const runtime = yield* Effect.promise(() => bootLocation(context, location, dependencies));
      if (!runtime) return;

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
