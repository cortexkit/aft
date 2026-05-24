import { z } from "zod";

export const NpmPackageEnvelopeSchema = z.object({
  "dist-tags": z.record(z.string(), z.string()).optional().default({}),
});

export const OpencodePluginTupleSchema = z.tuple([z.string(), z.record(z.string(), z.unknown())]);

export const OpencodeConfigSchema = z.object({
  plugin: z.array(z.union([z.string(), OpencodePluginTupleSchema])).optional(),
});

export const PackageJsonSchema = z
  .object({
    name: z.string().optional(),
    version: z.string().optional(),
    dependencies: z.record(z.string(), z.string()).optional(),
  })
  .passthrough();

export interface AutoUpdateCheckerOptions {
  enabled?: boolean;
  showStartupToast?: boolean;
  autoUpdate?: boolean;
  npmRegistryUrl?: string;
  fetchTimeoutMs?: number;
  signal?: AbortSignal;
  /**
   * Plugin-storage directory for the on-disk last-checked timestamp.
   * Multi-project plugin reloads coordinate through this file so npm
   * is hit at most once per `checkIntervalMs` window across all
   * concurrent plugin instances on the same machine.
   */
  storageDir?: string;
  /**
   * Minimum time between npm registry checks across all plugin
   * instances. Honored via the on-disk timestamp file. Default: 1 hour.
   */
  checkIntervalMs?: number;
  /**
   * Delay before the first check after plugin init. Default: 5s — gives
   * OpenCode time to finish its own startup work and avoids racing TUI
   * boot.
   */
  initDelayMs?: number;
}

export interface PluginEntryInfo {
  entry: string;
  isPinned: boolean;
  pinnedVersion: string | null;
  configPath: string;
}

export type NpmPackageEnvelope = z.infer<typeof NpmPackageEnvelopeSchema>;
export type OpencodeConfig = z.infer<typeof OpencodeConfigSchema>;
export type PackageJson = z.infer<typeof PackageJsonSchema>;
