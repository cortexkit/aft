/**
 * The OpenCode side of the config error state (see `config-error.ts` in
 * `@cortexkit/aft-bridge`): AFT's normal tool surface, with every tool call
 * failing with the configuration error instead of reaching a bridge.
 */

import { AftConfigError, ConfigErrorTransportPool } from "@cortexkit/aft-bridge";
import type { ToolDefinition } from "@opencode-ai/plugin";

import type { AftConfig } from "./config.js";
import { buildAftToolDefinitions, openCodeHashlineEffective } from "./tool-registration.js";
import type { PluginContext } from "./types.js";

type ToolMapBuilder = (ctx: PluginContext, config: AftConfig) => Record<string, ToolDefinition>;

/**
 * Replace every tool's `execute` with one that throws `message`. A thrown
 * error is how both OpenCode generations mark a tool call as failed, so the
 * call shows as an error rather than a successful result carrying the text.
 */
export function failEveryToolCall(
  tools: Readonly<Record<string, ToolDefinition>>,
  message: string,
): Record<string, ToolDefinition> {
  const failing: Record<string, ToolDefinition> = {};
  for (const [name, definition] of Object.entries(tools)) {
    failing[name] = {
      ...definition,
      execute: async () => {
        throw new AftConfigError(message);
      },
    };
  }
  return failing;
}

/**
 * Build the tool surface `config` selects, with every call failing with
 * `message`. The tool factories receive a transport pool that never starts a
 * bridge, so building the surface cannot spawn anything either.
 */
export function buildConfigErrorToolMap(
  config: AftConfig,
  message: string,
  client: unknown,
  buildToolMap: ToolMapBuilder = buildAftToolDefinitions,
): Record<string, ToolDefinition> {
  const ctx: PluginContext = {
    pool: new ConfigErrorTransportPool(message),
    client: client as PluginContext["client"],
    config,
    hashlineEffective: openCodeHashlineEffective(config),
    storageDir: "",
    isProjectEnabled: () => false,
  };
  return failEveryToolCall(buildToolMap(ctx, config), message);
}
