import type { BridgePool } from "@cortexkit/aft-bridge";
import type { PluginInput } from "@opencode-ai/plugin";
import type { AftConfig } from "./config.js";

interface ShellEnvPluginHost {
  trigger?: (
    name: "shell.env",
    context: { cwd: string; sessionID?: string; callID?: string },
    input: { env: Record<string, string> },
  ) => Promise<{ env?: Record<string, string> }> | { env?: Record<string, string> };
}

/**
 * Shared context passed to all tool factory functions.
 * Bundles the binary bridge, the OpenCode SDK client, and plugin config.
 */
export interface PluginContext {
  pool: BridgePool;
  client: PluginInput["client"];
  plugin?: ShellEnvPluginHost;
  config: AftConfig;
  /** Absolute path to AFT's storage dir (e.g. ~/.local/share/opencode/storage/plugin/aft) */
  storageDir: string;
}
