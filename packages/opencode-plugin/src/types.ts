import type { AftTransportPool, HostToolNames } from "@cortexkit/aft-bridge";
import type { PluginInput } from "@opencode-ai/plugin";
import type { Effect } from "effect";
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
  pool: AftTransportPool;
  client: PluginInput["client"];
  readSearchMessageV2?: (input: {
    sessionID: string;
    messageID: string;
  }) => Effect.Effect<unknown, unknown>;
  plugin?: ShellEnvPluginHost;
  config: AftConfig;
  /** Whether hashline edit/read mode is active for this plugin registration. */
  hashlineEffective?: boolean;
  /**
   * What this host calls AFT's command and patch tools, sent on every request
   * so AFT's reply text names them. Set by the OpenCode 2 runtime only; absent
   * leaves AFT's own names.
   */
  hostToolNames?: HostToolNames;
  /** Absolute path to AFT's storage dir (e.g. ~/.local/share/opencode/storage/plugin/aft) */
  storageDir: string;
  /** Returns false when user and project config merge to `enabled: false` for this project. */
  isProjectEnabled?: (projectRoot: string) => boolean;
}
