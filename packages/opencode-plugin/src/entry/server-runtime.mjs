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
import { startOpenCodeLiveConfigReload } from "../config-live-reload.js";
import { debug, log, warn } from "../logger.js";
import { resolvePluginVersion } from "../plugin-version.js";
import { registerAftConfigErrorRpc, registerAftRpc } from "../rpc/register.js";
import { hoistedV2ToolConsumers, v2PromptChannelFor } from "../tools/hoisted/v2.js";
import { registerV2HostToolOverlapNotice } from "../v2-host-tool-overlap.js";
import { registerV2PromptDetachHook } from "../v2-prompt-detach.js";
import { registerV2ToolHooks } from "../v2-tool-hooks.js";
import { registerV2WorkflowHints } from "../v2-workflow-hints.js";
import { createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import { buildHintsForRegisteredTools } from "../workflow-hints.js";
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
  registerPromptHook: registerV2PromptDetachHook,
  registerToolHooks: registerV2ToolHooks,
  registerWorkflowHints: registerV2WorkflowHints,
  registerHostToolOverlapNotice: registerV2HostToolOverlapNotice,
  // The prompt server comes from the user-only `opencode` block of the
  // Location's config; without it AFT finds the server it runs inside.
  toolConsumers: (context, config) => ({
    ...hoistedV2ToolConsumers(context, v2PromptChannelFor(config?.opencode)),
    ...createV2RuntimeConsumer(context),
  }),
  acquireBridge,
  releaseBridge,
  startLiveConfigReload: startOpenCodeLiveConfigReload,
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
  const consumers = dependencies.toolConsumers(context, config);
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
  const hintsBlock = buildHintsForRegisteredTools(config, registeredTools, hashlineEditRegistered);
  // Keep this Location's live config keys current when a config file changes.
  // The finalizer that releases the bridge also stops the watch.
  const liveConfigReload = dependencies.startLiveConfigReload({
    directory,
    initialSources: bootstrap.sources ?? [],
    initialSourceTexts: bootstrap.sourceTexts,
    getConfig: () => toolContext.config,
    setConfig: (next) => {
      toolContext.config = next;
    },
    notify,
  });
  return { consumers, pool, tools, liveConfigReload, toolContext, directory, hintsBlock };
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
          runtime.liveConfigReload?.stop();
          runtime.consumers.dispose?.();
          await dependencies.releaseBridge(runtime.pool);
        }),
      );
      const rpc = yield* dependencies.registerRpc(context, location, runtime.pool);
      yield* Effect.addFinalizer(() => Effect.promise(() => rpc.dispose()));
      // OpenCode 2 has no chat.message hook; its session prompt hook is where a
      // new message detaches a waiting bash, as chat.message does on OpenCode 1.
      // The config is read through the tool context so live reloads apply.
      // The bridge asked first is the one this Location's tools run on, the
      // Location's own directory; for a Location in a linked git worktree that
      // is the worktree, not the canonical main checkout the pool was acquired
      // for (issue #387). Other bridges are still tried after it.
      yield* dependencies.registerPromptHook(context, {
        pool: runtime.pool,
        projectRoot: runtime.directory,
        getConfig: () => runtime.toolContext.config,
        registerSession: runtime.consumers.registerSession,
      });
      yield* dependencies.registerTools(
        context,
        location,
        runtime.tools,
        runtime.consumers,
      );
      yield* dependencies.registerToolHooks(context, runtime.toolContext, new Set(Object.keys(runtime.tools)));
      yield* dependencies.registerWorkflowHints(context, runtime.hintsBlock);
      // Tell the user once, in the chat, when the host's own patch or shell
      // tool still runs beside AFT's apply_patch or bash. Checked on the first
      // prompt, once every plugin has registered its tools.
      yield* dependencies.registerHostToolOverlapNotice(
        context,
        new Set(Object.keys(runtime.tools)),
      );
    });
  };
}

export const serverEffect = makeServerEffect();
