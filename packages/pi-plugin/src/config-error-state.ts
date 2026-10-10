/**
 * The Pi side of the config error state (see `config-error.ts` in
 * `@cortexkit/aft-bridge`). An unusable configuration used to make the
 * extension register nothing, or throw from its factory, which Pi records
 * only in its own log. Instead the extension registers AFT's tool surface and
 * every call fails with the error and its fix; no binary is resolved and no
 * bridge is started.
 */

import {
  AftConfigError,
  ConfigErrorTransportPool,
  DEFAULT_DISABLED_TOOLS,
  formatConfigErrorMessage,
  formatConfigErrorStatusLine,
  formatConfigParseErrorMessage,
  subcConnectionFileError,
} from "@cortexkit/aft-bridge";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

import {
  type AftConfig,
  type ConfigLoadError,
  getConfigLoadErrors,
  getConfigLoadSources,
  getConfigLoadTexts,
  loadAftConfig,
} from "./config.js";
import { error, log } from "./logger.js";
import { registerPiToolSurface, resolvePiToolSurface } from "./tool-registration.js";
import type { PluginContext } from "./types.js";

/** Status-bar key the config error is shown under. */
export const CONFIG_ERROR_STATUS_KEY = "aft";

export type PiBootstrapConfig =
  | {
      ok: true;
      config: AftConfig;
      /** The config files this load read; a live config reload starts from them. */
      sources?: readonly string[];
      /** The text of each file this load read, keyed by path. */
      sourceTexts?: Readonly<Record<string, string>>;
    }
  | {
      ok: false;
      /** The error, its fix, and the note that a restart is needed. */
      message: string;
      /** Config whose tool surface is registered: the loaded one when usable, else the default. */
      config: AftConfig;
    };

/** Side effects of the config load, replaceable in tests. */
export interface PiBootstrapDependencies {
  loadConfig(directory: string): AftConfig;
  configLoadErrors(): readonly ConfigLoadError[];
  /** The config files the most recent `loadConfig` call read. */
  configLoadSources?(): readonly string[];
  /** The text of each file the most recent `loadConfig` call read. */
  configLoadTexts?(): ReadonlyMap<string, string>;
  /** Moves legacy config files into the CortexKit layout; returns user-facing warnings. */
  migrateConfigLocations(directory: string): string[];
  subcConnectionFileError(subcConnectionFile: string | undefined): Promise<string | null>;
}

const defaultDependencies: PiBootstrapDependencies = {
  loadConfig: loadAftConfig,
  configLoadErrors: getConfigLoadErrors,
  configLoadSources: getConfigLoadSources,
  configLoadTexts: getConfigLoadTexts,
  // Moving old per-harness aft.jsonc files to shared CortexKit paths requires
  // an explicit relocation operation, never an ordinary config load.
  migrateConfigLocations: () => [],
  subcConnectionFileError,
};

function defaultSurfaceConfig(): AftConfig {
  return { disabled_tools: [...DEFAULT_DISABLED_TOOLS] };
}

/** Log the error once at ERROR and report it once; never fall back to defaults. */
function configErrorState(
  detail: string,
  notify: (message: string) => void,
  surfaceConfig: AftConfig = defaultSurfaceConfig(),
): PiBootstrapConfig {
  const message = formatConfigErrorMessage(detail);
  error(message);
  notify(message);
  return { ok: false, message, config: surfaceConfig };
}

function parseFailure(dependencies: PiBootstrapDependencies): string | null {
  const [failure] = dependencies.configLoadErrors();
  return failure ? formatConfigParseErrorMessage(failure.path, failure.message) : null;
}

/**
 * Load the config for `directory` without writing files in production. An
 * injected location migration hook is only for callers that explicitly opt
 * into relocation; when present, the second load reads the relocated files.
 * A rejected configuration, a file that does not parse, any other load
 * failure, or a configured subc connection file that does not exist yields
 * the config error state. Migration is skipped when the first load fails.
 */
export async function resolvePiBootstrapConfig(
  directory: string,
  notify: (message: string) => void,
  dependencies: PiBootstrapDependencies = defaultDependencies,
): Promise<PiBootstrapConfig> {
  let config: AftConfig;
  let sources: readonly string[] = [];
  let sourceTexts: Record<string, string> = {};
  try {
    dependencies.loadConfig(directory);
    const firstFailure = parseFailure(dependencies);
    if (firstFailure) return configErrorState(firstFailure, notify);
    for (const message of dependencies.migrateConfigLocations(directory)) notify(message);
    config = dependencies.loadConfig(directory);
    const failure = parseFailure(dependencies);
    if (failure) return configErrorState(failure, notify);
    sources = [...(dependencies.configLoadSources?.() ?? [])];
    sourceTexts = Object.fromEntries(dependencies.configLoadTexts?.() ?? []);
  } catch (err) {
    return configErrorState(err instanceof Error ? err.message : String(err), notify);
  }
  const missing = await dependencies.subcConnectionFileError(config.subc?.connection_file);
  return missing === null
    ? { ok: true, config, sources, sourceTexts }
    : configErrorState(missing, notify, config);
}

/**
 * A view of `pi` whose `registerTool` swaps each tool's `execute` for one that
 * throws `message`. Pi turns a thrown error into an error tool result, so the
 * call is marked failed rather than returning the text as a success.
 */
function failingRegistrations(pi: ExtensionAPI, message: string): ExtensionAPI {
  const registerTool = ((tool: Parameters<ExtensionAPI["registerTool"]>[0]) => {
    pi.registerTool({
      ...tool,
      execute: async () => {
        throw new AftConfigError(message);
      },
    });
  }) as ExtensionAPI["registerTool"];
  return new Proxy(pi, {
    get(target, prop, receiver) {
      if (prop === "registerTool") return registerTool;
      return Reflect.get(target, prop, receiver);
    },
  });
}

/**
 * Register the config error state: the tool surface `config` selects with
 * every call failing, and the error on one line in Pi's status bar. Commands
 * and lifecycle hooks are not registered; there is no bridge for them.
 */
export function registerPiConfigErrorState(
  pi: ExtensionAPI,
  config: AftConfig,
  message: string,
): void {
  const ctx: PluginContext = {
    pool: new ConfigErrorTransportPool(message),
    config,
    hashlineEffective: false,
    storageDir: "",
  };
  registerPiToolSurface(failingRegistrations(pi, message), ctx, resolvePiToolSurface(config, pi));
  const statusLine = formatConfigErrorStatusLine(message);
  (
    pi.on as (
      event: "session_start",
      handler: (_event: unknown, extCtx?: { ui?: { setStatus?: StatusSetter } }) => void,
    ) => void
  )("session_start", (_event, extCtx) => {
    try {
      extCtx?.ui?.setStatus?.(CONFIG_ERROR_STATUS_KEY, statusLine);
    } catch {
      // A host without a status bar still fails every tool call with the error.
    }
  });
  log("AFT is in the config error state; every tool call will fail");
}

type StatusSetter = (key: string, text: string | undefined) => void;
