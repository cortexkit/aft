/**
 * The per-plugin environment work that feeds every AFT bridge, shared by the
 * OpenCode 1 plugin entry (`index.ts`) and the OpenCode 2 runtime entry
 * (`entry/server-runtime.mjs`).
 *
 * Both entries used to assemble this state separately, and the OpenCode 2
 * entry silently skipped most of it: it never resolved ONNX Runtime, so every
 * bridge it spawned fell back to a bare `dlopen("libonnxruntime.dylib")` and
 * semantic search failed on any host without `ORT_DYLIB_PATH` exported. Keeping
 * the whole sequence in this module means an entry can only differ in the
 * host-specific parts (how warnings reach the user, which directory it boots
 * for), not in what the bridge is configured with.
 *
 * What lives here:
 *   - loading the AFT config after migrating legacy config file locations,
 *     refusing a rejected configuration and delivering migration notices;
 *   - resolving the `aft` binary (with a background download when uncached);
 *   - the one-time storage migration into the shared CortexKit root;
 *   - the flat configure overrides: config tiers, `storage_dir`,
 *     `bash_permissions`, `harness`, and the LSP install cache
 *     (`lsp_auto_install_binaries`, `lsp_paths_extra`, `lsp_inflight_installs`);
 *   - ONNX Runtime resolution, patched in as `_ort_dylib_dir` once it settles;
 *   - the version-mismatch upgrade handler and per-project config loader the
 *     transport pool is built with;
 *   - the registration-dependent overrides `edit_slot_survives` and
 *     `aft_search_registered`, applied once the final tool map is known;
 *   - the registration warnings every entry reports the same way: one
 *     aggregated unknown-name warning per load and the hashline downgrade.
 */

import {
  type AftTransportPool,
  ensureBinary,
  ensureOnnxRuntime,
  ensureStorageMigrated,
  findBinary,
  findBinarySync,
  getManualInstallHint,
  isOrtAutoDownloadSupported,
  type PoolOptions,
  resolveCortexKitStorageRoot,
} from "@cortexkit/aft-bridge";

import {
  type AftConfig,
  buildConfigTierConfigureParams,
  ConfigRejectedError,
  deliverConfigLoadNotices,
  getConfigLoadNotices,
  loadAftConfig,
  migrateAftConfigLocations,
  resolvedIndexes,
} from "./config.js";
import { bridgeLogger, error, log, warn } from "./logger.js";
import { pushLspPathsAfterAutoInstall, runAutoInstall } from "./lsp-auto-install.js";
import { type AutoInstallPassLease, claimLspAutoInstallPass } from "./lsp-cache.js";
import { discoverRelevantGithubServers, runGithubAutoInstall } from "./lsp-github-install.js";
import { GITHUB_LSP_TABLE } from "./lsp-github-table.js";
import { NPM_LSP_TABLE } from "./lsp-npm-table.js";
import { openCodeHashlineDowngrade, openCodeHashlineEditRegistered } from "./tool-registration.js";

/** Delivers a user-visible warning through whatever channel the host entry has. */
export type BootstrapNotify = (message: string) => void;

/**
 * Replaceable side effects. Production uses {@link defaultBridgeBootstrapDependencies};
 * tests substitute fakes so a boot never downloads, spawns, or touches the
 * user's real config and storage.
 */
export interface BridgeBootstrapDependencies {
  loadConfig(directory: string): AftConfig;
  /**
   * Deliver the migration notices recorded by the most recent `loadConfig`
   * call, once per notice identity. Optional so a substituted `loadConfig`
   * does not deliver notices left over from a real load.
   */
  deliverLoadNotices?(notify: BootstrapNotify): void;
  /** Moves legacy config files into the CortexKit layout; returns user-facing warnings. */
  migrateConfigLocations(directory: string): string[];
  resolveBinary(version: string): Promise<string>;
  /** `binaryPath` is null when the subc daemon owns the binary; migration then resolves one only if it has work. */
  ensureStorageMigrated(binaryPath: string | null): Promise<void>;
  resolveStorageRoot(): string;
  buildConfigureParams(
    directory: string,
    processState: Record<string, unknown>,
  ): Record<string, unknown>;
  /** Returns the directory holding the ONNX Runtime library, or null. */
  ensureOnnxRuntime(storageDir: string): Promise<string | null>;
  /**
   * Seeds the LSP entries of `configOverrides` and starts background installs.
   * Resolves to the refreshed cache directories when installs ran, else null.
   */
  startLspAutoInstall(
    directory: string,
    config: AftConfig,
    configOverrides: Record<string, unknown>,
    notify: BootstrapNotify,
  ): Promise<string[] | null> | null;
  pushLspPaths(
    pool: Pick<AftTransportPool, "setConfigureOverride" | "reconfigure">,
    directory: string,
    paths: readonly string[],
  ): Promise<void>;
  isOrtAutoDownloadSupported(): boolean;
}

/**
 * Resolve the binary without ever executing one on the host thread.
 *
 * `findBinarySync` only accepts binaries whose identity is known without
 * running them (a versioned-cache entry with a matching identity sidecar, or
 * the npm platform package by its manifest version). When that misses, a
 * download starts in the background right away while `findBinary` checks the
 * remaining candidates on a worker thread. The resolver and the
 * first-tool-call path share ensureBinary's in-process promise; its filesystem
 * lock also coordinates a second host process without duplicate fetches.
 *
 * An explicit `AFT_BINARY_PATH` goes straight to `findBinary`, which verifies
 * its version off-thread and fails closed on a mismatch.
 */
async function resolveBinaryWithWarmup(version: string): Promise<string> {
  if (process.env.AFT_BINARY_PATH?.trim()) return findBinary(version);
  const trusted = findBinarySync(version);
  if (trusted) return trusted;
  void ensureBinary(version).then(
    (path) => {
      if (path) log(`Background binary warmup ready at ${path}`);
    },
    (err) => {
      warn(`Background binary warmup failed: ${err instanceof Error ? err.message : String(err)}`);
    },
  );
  return findBinary(version);
}

/**
 * One ONNX Runtime resolution per storage directory per process. An OpenCode 2
 * host boots one plugin runtime per Location, and a second concurrent
 * `ensureOnnxRuntime` call loses the install lock and returns null ("install
 * already in progress"), which would leave that Location without a runtime.
 * A null result is not cached, so a later boot can retry.
 */
const onnxResolutions = new Map<string, Promise<string | null>>();

function ensureOnnxRuntimeOncePerProcess(storageDir: string): Promise<string | null> {
  const existing = onnxResolutions.get(storageDir);
  if (existing) return existing;
  const resolution = ensureOnnxRuntime(storageDir).then(
    (dir) => {
      if (!dir) onnxResolutions.delete(storageDir);
      return dir;
    },
    (err) => {
      onnxResolutions.delete(storageDir);
      throw err;
    },
  );
  onnxResolutions.set(storageDir, resolution);
  return resolution;
}

/** Auto-install failures must never block plugin startup. */
function startLspAutoInstall(
  directory: string,
  config: AftConfig,
  configOverrides: Record<string, unknown>,
  notify: BootstrapNotify,
): Promise<string[] | null> | null {
  // Discover which LSPs the project needs and surface every already-cached
  // binary directory to Rust as `lsp_paths_extra`; the Rust resolver checks
  // that list after project-local node_modules and before PATH. Servers that
  // are not cached yet install in the background (npm packages, or GitHub
  // releases for the heavier native servers) behind the `lsp.grace_days`
  // window that defends against newly-published malicious versions.
  let lease: AutoInstallPassLease | null = null;
  try {
    const autoInstall = config.lsp?.auto_install ?? true;
    const graceDays = config.lsp?.grace_days ?? 7;
    const versions = config.lsp?.versions ?? {};
    const disabled = new Set(config.lsp?.disabled ?? []);
    lease = autoInstall ? claimLspAutoInstallPass() : null;
    const skippedByRecentAutoInstall = autoInstall && lease === null;
    if (skippedByRecentAutoInstall) {
      log("[lsp] skipping auto-install (another instance ran one recently)");
    }
    const runSharedAutoInstall = autoInstall && !skippedByRecentAutoInstall;
    // With `lsp.auto_install: false` the list stays empty so Rust's configure
    // skips its built-in missing-binary walk; otherwise users who opted out of
    // auto-install got `lsp_binary_missing` warnings on every configure.
    // Explicit `lsp.servers` entries still warn: those are user-configured.
    configOverrides.lsp_auto_install_binaries = autoInstall
      ? [...new Set([...NPM_LSP_TABLE, ...GITHUB_LSP_TABLE].map((spec) => spec.binary))]
      : [];

    const npmResult = runAutoInstall(directory, {
      autoInstall: runSharedAutoInstall,
      graceDays,
      versions,
      disabled,
    });
    // GitHub-distributed servers gate on relevance separately because the
    // binaries are heavier (10-100 MB).
    const ghResult = runGithubAutoInstall(discoverRelevantGithubServers(directory), {
      autoInstall: runSharedAutoInstall,
      graceDays,
      versions,
      disabled,
    });

    const mergedBinDirs = [...npmResult.cachedBinDirs, ...ghResult.cachedBinDirs];
    if (mergedBinDirs.length > 0) configOverrides.lsp_paths_extra = mergedBinDirs;
    const inflight = [
      ...new Set([...npmResult.installingBinaries, ...ghResult.installingBinaries]),
    ];
    if (inflight.length > 0) configOverrides.lsp_inflight_installs = inflight;
    const installsWereStarted = npmResult.installsStarted > 0 || ghResult.installsStarted > 0;
    if (installsWereStarted) {
      log(
        `[lsp] auto-install: ${npmResult.installsStarted} npm + ${ghResult.installsStarted} github install(s) running in background`,
      );
    }

    // Once installs settle, refresh the cached directories and send ONE
    // summary listing only reasons the user can act on ("grace blocked":
    // pin a version; "install failed": check the log), never routine skips.
    const completion = Promise.all([npmResult.installsComplete, ghResult.installsComplete])
      .then(() => {
        if (!installsWereStarted && !skippedByRecentAutoInstall) return null;
        const updatedPaths = [
          ...new Set([...npmResult.getCachedBinDirs(), ...ghResult.getCachedBinDirs()]),
        ];
        if (updatedPaths.length > 0) configOverrides.lsp_paths_extra = updatedPaths;
        else delete configOverrides.lsp_paths_extra;
        return updatedPaths;
      })
      .then((updatedPaths) => {
        const routine = new Set([
          "auto_install: false",
          "disabled by config",
          "not relevant to project",
          "already installed",
          "another install in progress",
        ]);
        const actionable = [...npmResult.skipped, ...ghResult.skipped].filter(
          (s) => !routine.has(s.reason.toLowerCase()),
        );
        if (actionable.length > 0) {
          const lines = actionable.map((s) => `  • ${s.id}: ${s.reason}`).join("\n");
          notify(
            `AFT skipped or failed to install ${actionable.length} LSP server(s):\n${lines}\n\n` +
              "See `/aft-status` for details, or check the plugin log. " +
              'Pin a working version with `lsp.versions: { "<package>": "<version>" }` if grace is blocking, ' +
              "or set `lsp.auto_install: false` to suppress this entirely.",
          );
        }
        return updatedPaths;
      })
      .catch((err) => {
        warn(`[lsp] install-summary aggregation failed: ${err}`);
        return null;
      })
      .finally(() => {
        lease?.release();
        lease = null;
      });
    return installsWereStarted || skippedByRecentAutoInstall ? completion : null;
  } catch (err) {
    lease?.release();
    lease = null;
    warn(`[lsp] auto-install setup failed: ${err instanceof Error ? err.message : String(err)}`);
    return null;
  }
}

export const defaultBridgeBootstrapDependencies: BridgeBootstrapDependencies = {
  loadConfig: loadAftConfig,
  deliverLoadNotices: (notify) => deliverConfigLoadNotices(notify, getConfigLoadNotices()),
  migrateConfigLocations: (directory) =>
    migrateAftConfigLocations(directory, bridgeLogger).flatMap((result) => result.warnings),
  resolveBinary: resolveBinaryWithWarmup,
  ensureStorageMigrated: (binaryPath) =>
    ensureStorageMigrated({
      harness: "opencode",
      binaryPath: binaryPath ?? undefined,
      logger: bridgeLogger,
    }),
  resolveStorageRoot: resolveCortexKitStorageRoot,
  buildConfigureParams: buildConfigTierConfigureParams,
  ensureOnnxRuntime: ensureOnnxRuntimeOncePerProcess,
  startLspAutoInstall,
  pushLspPaths: pushLspPathsAfterAutoInstall,
  isOrtAutoDownloadSupported,
};

/**
 * Load `directory`'s config, or null when it is rejected (a retired key after
 * its migration window, or an already retired GitHub alias). The rejection is
 * reported through `notify`; other load errors propagate.
 */
function loadConfigOrNull(
  directory: string,
  notify: BootstrapNotify,
  dependencies: BridgeBootstrapDependencies,
): AftConfig | null {
  try {
    return dependencies.loadConfig(directory);
  } catch (err) {
    if (!(err instanceof ConfigRejectedError)) throw err;
    error(err.message);
    notify(err.message);
    return null;
  }
}

/**
 * Load the config for `directory`, migrating legacy config file locations
 * first. Returns null when the configuration is rejected: a rejected project
 * publishes no AFT registrations and does no binary, storage or index work,
 * so migration is skipped too. Migration notices for retired keys that are
 * still translated are delivered through `notify`, once per notice identity.
 */
export function loadBootstrapConfig(
  directory: string,
  notify: BootstrapNotify,
  dependencies: BridgeBootstrapDependencies = defaultBridgeBootstrapDependencies,
): AftConfig | null {
  if (loadConfigOrNull(directory, notify, dependencies) === null) return null;
  for (const message of dependencies.migrateConfigLocations(directory)) notify(message);
  // Reload: migration may have moved the file the first read came from.
  const config = loadConfigOrNull(directory, notify, dependencies);
  if (config === null) return null;
  dependencies.deliverLoadNotices?.(notify);
  return config;
}

/**
 * Answer "may AFT work in this project?" for projects other than the one an
 * entry booted for. There is no config switch that turns AFT off; only a
 * rejected configuration does, until it is fixed. Answers are cached per
 * project and a rejection is logged once.
 */
export function createProjectAcceptance(
  bootRoot: string,
  dependencies: Pick<
    BridgeBootstrapDependencies,
    "loadConfig"
  > = defaultBridgeBootstrapDependencies,
): (projectRoot: string) => boolean {
  const accepted = new Map<string, boolean>([[bootRoot, true]]);
  return (projectRoot) => {
    const cached = accepted.get(projectRoot);
    if (cached !== undefined) return cached;
    let ok = true;
    try {
      dependencies.loadConfig(projectRoot);
    } catch (err) {
      if (!(err instanceof ConfigRejectedError)) throw err;
      error(err.message);
      log(`AFT disabled for ${projectRoot}: its configuration was rejected`);
      ok = false;
    }
    accepted.set(projectRoot, ok);
    return ok;
  };
}

export interface BridgeEnvironmentOptions {
  /** Project whose config tiers seed the flat configure payload. */
  configRoot: string;
  /** Directory scanned to decide which LSP servers are relevant. */
  lspDirectory: string;
  config: AftConfig;
  pluginVersion: string;
  notify: BootstrapNotify;
}

export interface BridgeEnvironment {
  storageDir: string;
  /**
   * Resolved `aft` binary for the standalone bridge, or null when the config
   * selects the subc daemon (which runs its own binary).
   */
  binaryPath: string | null;
  /** Flat configure overrides to construct the transport pool with. */
  configOverrides: Record<string, unknown>;
  /** Pending ONNX Runtime directory, or null when semantic search does not need it. */
  onnxRuntime: Promise<string | null> | null;
  /**
   * Wire the asynchronous results into the pool once it exists: the ONNX
   * Runtime directory as `_ort_dylib_dir`, and LSP directories that finished
   * installing during startup.
   */
  attach(pool: Pick<AftTransportPool, "setConfigureOverride" | "reconfigure">): void;
}

export async function prepareBridgeEnvironment(
  options: BridgeEnvironmentOptions,
  dependencies: BridgeBootstrapDependencies = defaultBridgeBootstrapDependencies,
): Promise<BridgeEnvironment> {
  const { config, notify } = options;
  // With a subc connection file the daemon runs the binary and the plugin
  // never spawns one (the transport factory fails loud instead of falling back
  // to a standalone bridge), so resolving a local binary would be wasted work.
  const usesSubc = Boolean(config.subc?.connection_file?.trim());
  const binaryPath = usesSubc ? null : await dependencies.resolveBinary(options.pluginVersion);
  // Must complete before anything reads or writes the storage root.
  await dependencies.ensureStorageMigrated(binaryPath);

  // Flat params are plugin-computed process state shared by every bridge.
  // Core-domain config flows only through the `config` tiers; per-project
  // tiers are reloaded at bridge spawn by the pool's project config loader.
  const storageDir = dependencies.resolveStorageRoot();
  const configOverrides = dependencies.buildConfigureParams(options.configRoot, {
    bash_permissions: true,
    harness: "opencode",
    storage_dir: storageDir,
  });

  // ONNX Runtime is resolved in the background: the archive is 60-80 MB and
  // awaiting it made hosts appear to hang on a slow connection. A cached or
  // system runtime resolves within a few ticks, well before the first lazy
  // bridge spawn; a download patches `_ort_dylib_dir` in when it finishes, and
  // only bridges spawned after that point load it.
  let onnxRuntime: Promise<string | null> | null = null;
  const fastembed = (config.semantic?.backend ?? "fastembed") === "fastembed";
  if (resolvedIndexes(config).semantic && fastembed) {
    onnxRuntime = dependencies.ensureOnnxRuntime(storageDir).catch((err) => {
      warn(
        `ONNX Runtime setup failed: ${err instanceof Error ? err.message : String(err)}. Semantic search will be unavailable.`,
      );
      return null;
    });
  }

  const lspCompletion = dependencies.startLspAutoInstall(
    options.lspDirectory,
    config,
    configOverrides,
    notify,
  );

  return {
    storageDir,
    binaryPath,
    configOverrides,
    onnxRuntime,
    attach(pool) {
      pool.setConfigureOverride("harness", "opencode");
      onnxRuntime?.then((ortDylibDir) => {
        if (ortDylibDir) {
          try {
            pool.setConfigureOverride("_ort_dylib_dir", ortDylibDir);
          } catch (err) {
            // The pool may have been released before a download finished.
            warn(`ONNX Runtime ready but the bridge pool is gone: ${err}`);
            return;
          }
          log(`ONNX Runtime ready at ${ortDylibDir}; new bridges will load semantic backend.`);
        } else if (!dependencies.isOrtAutoDownloadSupported()) {
          log(`ONNX Runtime auto-download not supported on ${process.platform}/${process.arch}.`);
          notify(`Semantic search requires ONNX Runtime.\nInstall: ${getManualInstallHint()}`);
        }
      });
      lspCompletion?.then((updatedPaths) => {
        if (!updatedPaths) return;
        dependencies
          .pushLspPaths(pool, options.lspDirectory, updatedPaths)
          .then(() => {
            log(
              `[lsp] lsp_paths_extra updated after auto-install: ${updatedPaths.length} dirs pushed to live bridges`,
            );
          })
          .catch((err) => {
            warn(`[lsp] live bridge lsp_paths_extra update failed: ${err}`);
          });
      });
    },
  };
}

/**
 * Pool options both entries share: the error prefix, the minimum binary
 * version, the coordinated upgrade when a bridge reports an older binary, and
 * the per-project config tiers loaded at each bridge spawn.
 */
export function createSharedPoolOptions(options: {
  pluginVersion: string;
  getPool(): Pick<AftTransportPool, "replaceBinary">;
  isProjectEnabled(projectRoot: string): boolean;
  notifyForRoot(projectRoot: string, message: string): void;
  dependencies?: BridgeBootstrapDependencies;
}): Pick<PoolOptions, "errorPrefix" | "minVersion" | "onVersionMismatch" | "projectConfigLoader"> {
  const dependencies = options.dependencies ?? defaultBridgeBootstrapDependencies;
  // Concurrent mismatches for one target version wait for the first
  // download and hot-swap instead of failing while it is still in flight.
  const upgrades = new Map<string, Promise<string | null>>();
  return {
    errorPrefix: "[aft-plugin]",
    minVersion: options.pluginVersion,
    // Each project's own config tiers win for its bridge. Without this, a host
    // serving many projects from one plugin instance would configure every
    // bridge with whichever project was visible at plugin startup.
    projectConfigLoader: (projectRoot) => {
      try {
        if (!options.isProjectEnabled(projectRoot)) return {};
        for (const message of dependencies.migrateConfigLocations(projectRoot)) {
          options.notifyForRoot(projectRoot, message);
        }
        return dependencies.buildConfigureParams(projectRoot, {});
      } catch (err) {
        warn(
          `readConfigTiers(${projectRoot}) failed; falling back to plugin-init config: ${
            err instanceof Error ? err.message : String(err)
          }`,
        );
        return {};
      }
    },
    onVersionMismatch: async (binaryVersion, minVersion) => {
      const existing = upgrades.get(minVersion);
      if (existing) {
        log(
          `Version ${binaryVersion} < ${minVersion}; awaiting in-flight compatible binary upgrade`,
        );
        return existing;
      }
      const upgrade = (async () => {
        warn(
          `WARNING: aft binary v${binaryVersion} is older than plugin v${minVersion}. ` +
            "Some features may not work. Attempting to download a compatible binary...",
        );
        try {
          const path = await ensureBinary(`v${minVersion}`);
          if (!path) {
            warn(`Could not find or download v${minVersion}. Continuing with v${binaryVersion}.`);
            return null;
          }
          log(`Found/downloaded compatible binary at ${path}. Replacing running bridges...`);
          const replaced = await options.getPool().replaceBinary(path);
          log("Binary replaced successfully. New bridges will use the updated binary.");
          // Returning the new path triggers aft-bridge's coordinated retry of the
          // in-flight request against the replacement binary.
          return replaced;
        } catch (err) {
          error(
            `Auto-download failed: ${(err as Error).message}. Install manually: cargo install agent-file-tools@${minVersion}`,
          );
          return null;
        } finally {
          upgrades.delete(minVersion);
        }
      })();
      upgrades.set(minVersion, upgrade);
      return upgrade;
    },
  };
}

/**
 * Apply the overrides that depend on the final registered tool surface.
 *
 * `edit_slot_survives` tells Rust whether the hashline edit arm is registered;
 * `aft_search_registered` lets the grep-rewrite footer point at `aft_search`
 * instead of the grep tool. Both reach lazily spawned bridges through the
 * pool's runtime overrides.
 */
export function applyToolSurfaceOverrides(
  pool: Pick<AftTransportPool, "setConfigureOverride">,
  config: AftConfig,
  registeredTools: ReadonlySet<string>,
): { hashlineEditRegistered: boolean; aftSearchRegistered: boolean } {
  const hashlineEditRegistered = openCodeHashlineEditRegistered(config, registeredTools);
  const aftSearchRegistered = registeredTools.has("aft_search");
  try {
    pool.setConfigureOverride("edit_slot_survives", hashlineEditRegistered);
  } catch (err) {
    // `edit_slot_survives` is write-once per pool. An OpenCode 2 host shares
    // one standalone pool between all Locations in the process, so every
    // Location after the first finds it already captured; the first value
    // stays in force. That is expected on every start, so it is logged at
    // info rather than raised as a warning.
    log(`edit_slot_survives not updated: ${err instanceof Error ? err.message : String(err)}`);
  }
  pool.setConfigureOverride("aft_search_registered", aftSearchRegistered);
  return { hashlineEditRegistered, aftSearchRegistered };
}

/**
 * The callback `buildAftToolDefinitions` reports unknown `disabled_tools`
 * names through: one aggregated warning per load. Unknown names stay inert
 * and are kept in the list.
 */
export function unknownDisabledToolsReporter(
  notify: BootstrapNotify,
): (unknown: readonly string[]) => void {
  return (unknown) => {
    const message = `unknown_disabled_tools: disabled_tools lists names AFT does not know: ${unknown.join(", ")}`;
    warn(message);
    notify(message);
  };
}

/**
 * Report a requested hashline surface that the registered tools cannot serve
 * (the tagged read or the edit slot is disabled). One warning per load; the
 * surviving slots keep their ordinary behavior. Returns the downgrade, if any.
 */
export function reportHashlineDowngrade(
  config: AftConfig,
  registeredTools: ReadonlySet<string>,
  notify: BootstrapNotify,
): ReturnType<typeof openCodeHashlineDowngrade> {
  const downgrade = openCodeHashlineDowngrade(config, registeredTools);
  if (downgrade) {
    warn(`${downgrade.code}: ${downgrade.message}`);
    notify(downgrade.message);
  }
  return downgrade;
}
