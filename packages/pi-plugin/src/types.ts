import type { BridgePool } from "@cortexkit/aft-bridge";
import type { AftConfig } from "./config.js";

/**
 * Shared context passed to every tool wrapper.
 * Bundles the bridge pool, the resolved AFT config, and the storage dir.
 *
 * Note: session ID is resolved per tool call from Pi's `ExtensionContext`
 * (`sessionManager.getSessionId()`) rather than stored here, so that
 * `/new`, `/fork`, and `/resume` each scope their own undo/checkpoint
 * state in AFT.
 */
export interface PluginContext {
  pool: BridgePool;
  config: AftConfig;
  /** Absolute path to AFT's storage dir (e.g. ~/.pi/agent/aft) */
  storageDir: string;
}
