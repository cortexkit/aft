import {
  acquireBridge,
  findBinary,
  releaseBridge,
  resolveCortexKitStorageRoot,
} from "@cortexkit/aft-bridge";
import { Effect } from "effect";
import { z } from "zod";

import {
  buildConfigTierConfigureParams,
  loadAftConfig,
  resolveBridgePoolTransportOptions,
} from "../config.js";
import { resolvePluginVersion } from "../plugin-version.js";
import { buildOpenCodeToolMap, openCodeHashlineEffective } from "../tool-registration.js";

const defaults = {
  buildConfigureParams: buildConfigTierConfigureParams,
  buildToolMap: buildOpenCodeToolMap,
  acquireBridge,
  loadConfig: loadAftConfig,
  releaseBridge,
  resolveBinary: findBinary,
  resolvePoolOptions: resolveBridgePoolTransportOptions,
  resolveStorageRoot: resolveCortexKitStorageRoot,
  resolveVersion: () => resolvePluginVersion(import.meta.url),
};

function failure(error) {
  return {
    _tag: "Tool.Error",
    message: error instanceof Error ? error.message : String(error),
    error,
  };
}

function resultContent(result) {
  if (typeof result === "string") return { content: result };

  const attachments = (result.attachments ?? []).map((attachment) => ({
    type: "file",
    uri: attachment.url,
    mime: attachment.mime,
    ...(attachment.filename ? { name: attachment.filename } : {}),
  }));
  const content = attachments.length
    ? [{ type: "text", text: result.output }, ...attachments]
    : result.output;
  const metadata = {
    ...(result.metadata ?? {}),
    ...(result.title ? { title: result.title } : {}),
  };
  return {
    content,
    ...(Object.keys(metadata).length ? { metadata } : {}),
  };
}

export function adaptV1Tool(name, definition, location) {
  const directory = location.directory;
  const worktree = location.project?.canonical ?? location.project?.directory ?? directory;

  return {
    name,
    description: definition.description,
    input: z.object(definition.args),
    execute: (input, context) =>
      Effect.tryPromise({
        try: (signal) =>
          definition.execute(input, {
            sessionID: context.sessionID,
            messageID: context.messageID,
            agent: context.agent,
            directory,
            worktree,
            abort: signal,
            metadata: (update) => {
              void Effect.runPromise(context.progress({
                ...(update.metadata ?? {}),
                ...(update.title ? { title: update.title } : {}),
              }));
            },
            ask: async () => {
              throw new Error("V2 permission requests are not supported by this compatibility adapter");
            },
          }),
        catch: failure,
      }).pipe(Effect.map(resultContent)),
  };
}

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
      yield* context.tool.transform((editor) => {
        for (const [name, definition] of Object.entries(runtime.tools)) {
          editor.add(adaptV1Tool(name, definition, location));
        }
      });
    });
  };
}

export const serverEffect = makeServerEffect();
