/**
 * The "config error" state shared by every plugin host.
 *
 * When AFT's configuration cannot be used (a rejected retired key, a config
 * file that does not parse, a subc connection file that does not exist, ...)
 * the plugins used to throw during initialization. Hosts react to that by not
 * loading the plugin at all and recording the reason only in their own log
 * file, so neither the user nor the model learns that AFT is missing.
 *
 * Instead, a plugin in this state still loads and registers AFT's tool
 * surface, but every tool call fails with the configuration error and its fix.
 * Nothing runs in AFT's place: no bridge is spawned, no binary is resolved and
 * no host tool is substituted, so the failure stays visible.
 */

import { homedir } from "node:os";
import { isAbsolute, join } from "node:path";

import type { BridgeToolCallRuntime } from "./pool.js";
import type {
  AftProjectTransport,
  AftTransportPool,
  ToolCallArguments,
  ToolCallOptions,
  ToolCallResult,
} from "./transport.js";

/**
 * Appended to every config error. The plugins read their configuration once,
 * at startup, so fixing the file does not take effect until the host restarts.
 */
export const CONFIG_ERROR_RESTART_NOTE =
  "AFT reads its config only at startup, so restart the host after fixing it.";

/** The full text a config-error tool call fails with: the error, its fix, and the restart note. */
export function formatConfigErrorMessage(detail: string): string {
  const trimmed = detail.trim();
  const sentence = /[.!?]$/.test(trimmed) ? trimmed : `${trimmed}.`;
  return `${sentence} ${CONFIG_ERROR_RESTART_NOTE}`;
}

/**
 * Where a configured `subc.connection_file` points: `~` and relative paths are
 * resolved against the home directory, never the project, because the file is
 * a per-machine daemon endpoint. Shared by the transport factory and `doctor`.
 */
export function resolveSubcConnectionFilePath(raw: string, home: string = homedir()): string {
  const trimmed = raw.trim();
  if (trimmed.startsWith("~")) return join(home, trimmed.slice(1).replace(/^[/\\]/, ""));
  if (isAbsolute(trimmed)) return trimmed;
  return join(home, trimmed);
}

/** The error for a configured subc connection file that does not exist, with its fix. */
export function formatSubcConnectionMissingMessage(raw: string, resolved: string): string {
  return (
    `subc.connection_file is set to "${raw.trim()}" (resolved: ${resolved}) but no subc ` +
    `connection file exists there. Start the Subconscious daemon, correct the path, ` +
    `or remove subc.connection_file from your user config to use the standalone bridge.`
  );
}

/**
 * Every configuration condition that puts a plugin in the config error state,
 * named the same way by the plugins and by `doctor`.
 */
export type ConfigErrorCode = "config_parse_error" | "config_rejected" | "subc_connection_missing";

/** Error text for a config file that does not parse, as the plugins and `doctor` report it. */
export function formatConfigParseErrorMessage(configPath: string, errorMessage: string): string {
  return (
    `AFT config at ${configPath} failed to parse: ${errorMessage}. ` +
    "Fix the JSONC syntax in that file, or run `npx @cortexkit/aft doctor`."
  );
}

/** One-line form for a status bar or sidebar row. */
export function formatConfigErrorStatusLine(message: string): string {
  return `AFT config error: ${message.replace(/\s+/g, " ").trim()}`;
}

/**
 * The status payload a plugin's status endpoint returns in the config error
 * state. `status` is not `not_initialized`, so status clients render it rather
 * than treating it as a placeholder and looking for another server.
 */
export function configErrorStatusSnapshot(message: string): Record<string, unknown> {
  return {
    success: true,
    status: "config_error",
    config_error: message,
    message: formatConfigErrorStatusLine(message),
  };
}

/** Thrown by every tool call and every bridge request made in the config error state. */
export class AftConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "AftConfigError";
  }
}

/**
 * A transport pool that never starts anything. Requests that would need a
 * bridge fail with the config error; lookups of live bridges report none, and
 * lifecycle calls do nothing, so host hooks that probe the pool stay quiet.
 */
export class ConfigErrorTransportPool implements AftTransportPool {
  constructor(readonly message: string) {}

  private fail(): never {
    throw new AftConfigError(this.message);
  }

  getBridge(_projectRoot: string): AftProjectTransport {
    return this.fail();
  }

  getActiveBridgeForRoot(_projectRoot: string): AftProjectTransport | null {
    return null;
  }

  activeBridges(): AftProjectTransport[] {
    return [];
  }

  async toolCall(
    _projectRoot: string,
    _runtime: BridgeToolCallRuntime,
    _name: string,
    _rawArgs?: ToolCallArguments,
    _options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    return this.fail();
  }

  setConfigureOverride(_key: string, _value: unknown): void {}

  async reconfigure(_projectRoot: string, _overrides: Record<string, unknown>): Promise<void> {}

  async replaceBinary(_path: string): Promise<string> {
    return this.fail();
  }

  isShutdown(): boolean {
    return false;
  }

  async shutdown(_reason?: string): Promise<void> {}

  async closeSession(_projectRoot: string, _session: string): Promise<void> {}
}
