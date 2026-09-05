import {
  acquireBridge,
  findBinary,
  releaseBridge,
  resolveCortexKitStorageRoot,
} from "@cortexkit/aft-bridge";
import { Effect } from "effect";

import {
  buildConfigTierConfigureParams,
  loadAftConfig,
  resolveBridgePoolTransportOptions,
} from "../config.js";
import { resolvePluginVersion } from "../plugin-version.js";
import {
  buildAftToolDefinitions,
  openCodeHashlineEffective,
  registerAftTools,
} from "../tool-registration.js";

const defaults = {
  buildConfigureParams: buildConfigTierConfigureParams,
  buildToolMap: buildAftToolDefinitions,
  registerTools: registerAftTools,
  toolConsumers: {},
  acquireBridge,
  loadConfig: loadAftConfig,
  releaseBridge,
  resolveBinary: findBinary,
  resolvePoolOptions: resolveBridgePoolTransportOptions,
  resolveStorageRoot: resolveCortexKitStorageRoot,
  resolveVersion: () => resolvePluginVersion(import.meta.url),
};

async function bootLocation(context, location, dependencies) {
  const directory = location.directory;
  const config = dependencies.loadConfig(directory);
  if (config.enabled === false) return undefined;

  const storageDir = dependencies.resolveStorageRoot();
  const configOverrides = dependencies.buildConfigureParams(directory, {
    bash_permissions: true,
    harness: "opencode",
    storage_dir: storageDir,
  });
  const binaryPath = await dependencies.resolveBinary(dependencies.resolveVersion());
  const canonicalDirectory = location.project?.canonical ?? directory;
  const pool = await dependencies.acquireBridge(canonicalDirectory, {
    harness: "opencode",
    binaryPath,
    poolOptions: dependencies.resolvePoolOptions(config),
    configOverrides,
    subcConnectionFile: config.subc?.connection_file,
  });
  const toolContext = {
    pool,
    client: context,
    config,
    hashlineEffective: openCodeHashlineEffective(config),
    storageDir,
    isProjectEnabled: (projectRoot) =>
      projectRoot === directory ? true : dependencies.loadConfig(projectRoot).enabled !== false,
  };
  const tools = dependencies.buildToolMap(toolContext, config);
  return { pool, tools };
}

export function makeServerEffect(overrides = {}) {
  const dependencies = { ...defaults, ...overrides };

  return (context) => {
    // The V2 host already resolved the Location before invoking the plugin. Capture
    // it now so every registration and transport route belongs to that exact scope.
    const location = context.location;

    return Effect.gen(function* () {
      const runtime = yield* Effect.promise(() => bootLocation(context, location, dependencies));
      if (!runtime) return;

      yield* Effect.addFinalizer(() =>
        Effect.promise(() => dependencies.releaseBridge(runtime.pool)),
      );
      yield* dependencies.registerTools(
        context,
        location,
        runtime.tools,
        dependencies.toolConsumers,
      );
    });
  };
}

export const serverEffect = makeServerEffect();
