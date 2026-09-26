/**
 * AFT (Agent File Tools) extension for Pi coding agent.
 *
 * Config is loaded from two levels (project overrides user):
 * - User:    ~/.config/cortexkit/aft.jsonc (XDG_CONFIG_HOME-aware)
 * - Project: <project>/.cortexkit/aft.jsonc
 *
 * Tools registered:
 *
 * Hoisting (replace Pi's built-in tools):
 *   - read   → AFT's indexed Rust reader
 *   - write  → AFT's atomic writer with backup + auto-format + LSP diagnostics
 *   - edit   → AFT's fuzzy-match edit with backup + diagnostics
 *   - grep   → AFT's trigram-indexed grep (falls back to ripgrep outside project root)
 *
 * AFT-specific:
 *   - aft_outline    Structural outline (symbols, headings) for files/URLs
 *   - aft_zoom       Symbol-level inspection with call-graph annotations
 *   - aft_search     Unified lexical/semantic search
 *   - aft_callgraph   Call-graph navigation (callers, call_tree, impact, trace_to, trace_to_symbol, trace_data)
 *   - aft_conflicts  One-call merge conflict inspection
 *   - aft_import     Language-aware import add/remove/organize
 *   - aft_safety     Per-file undo, checkpoints, restore
 *   - aft_delete     Delete file with backup
 *   - aft_move       Move/rename file
 *   - ast_grep_search / ast_grep_replace  AST-aware pattern search/rewrite
 *
 * Commands:
 *   - /aft-status    Status dialog (index states, LSP servers, storage dir)
 */

import { createRequire } from "node:module";
import {
  type AftTransportPool,
  canonicalizeProjectRoot,
  createAftTransportPool,
  ensureBinary,
  ensureOnnxRuntime,
  ensureStorageMigrated,
  findBinary,
  findBinarySync,
  formatDroppedKeyWarnings,
  getManualInstallHint,
  getOnnxRuntimeInstallFailure,
  isHomeDirectoryRoot,
  resolveCortexKitStorageRoot,
  resolveIndexes,
  setActiveLogger,
  unknownDisabledTools,
} from "@cortexkit/aft-bridge";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import {
  appendToolResultBgCompletions,
  getActiveSessionId,
  handlePushedBgCompletion,
  handlePushedBgLongRunning,
  handlePushedPatternMatch,
  handleSubcBgEventsNudge,
  handleTurnEndBgCompletions,
  setActiveSessionId,
} from "./bg-notifications.js";
import { registerStatusCommand } from "./commands/aft-status.js";
import {
  type AftConfig,
  buildConfigTierConfigureParams,
  deliverConfigLoadNotices,
  formatConfigParseFailureMessage,
  getConfigLoadErrors,
  resolveBridgePoolTransportOptions,
} from "./config.js";
import { bridgeLogger, error, flushLogs, log, warn } from "./logger.js";
import {
  abortInFlightAutoInstalls,
  pushLspPathsAfterAutoInstall,
  runAutoInstall,
} from "./lsp-auto-install.js";
import { type AutoInstallPassLease, claimLspAutoInstallPass } from "./lsp-cache.js";
import {
  abortInFlightGithubInstalls,
  discoverRelevantGithubServers,
  runGithubAutoInstall,
} from "./lsp-github-install.js";
import { GITHUB_LSP_TABLE } from "./lsp-github-table.js";
import { NPM_LSP_TABLE } from "./lsp-npm-table.js";
import {
  type ConfigureWarning,
  deliverConfigureWarnings,
  sendFeatureAnnouncement,
} from "./notifications.js";

// Register our logger with @cortexkit/aft-bridge before any bridge code runs.
setActiveLogger(bridgeLogger);

import {
  shouldDetachBashWaitOnUserMessage,
  signalBashWaitDetachForProject,
  stripUserMessageDetachKeyword,
} from "./bash-wait-detach.js";
import { registerPiConfigErrorState, resolvePiBootstrapConfig } from "./config-error-state.js";
import { recordActiveExtensionApi } from "./harness.js";
import { MAGIC_CONTEXT_SUBAGENT_ENV, skipsEagerStartup } from "./session-kind.js";
import { registerShutdownCleanup } from "./shutdown-hooks.js";
import { signalSyncWatchAbort } from "./sync-watch-abort.js";
import {
  piHashlineDowngrade,
  piHashlineEffective,
  registerPiToolSurface,
  resolvePiToolSurface,
} from "./tool-registration.js";
import { resolveSessionId } from "./tools/_shared.js";
import { registerBashTool } from "./tools/bash.js";
import type { PluginContext } from "./types.js";
import { registerWorkflowHints } from "./workflow-hints.js";

type BashLongRunningPayload = {
  session_id: string;
  task_id: string;
  command: string;
  elapsed_ms: number;
  mode?: "pipes" | "pty" | string;
};

type BashPatternMatchPayload = {
  session_id: string;
  task_id: string;
  watch_id: string;
  match_text: string;
  match_offset: number;
  context: string;
  once: boolean;
  reason?: "pattern_match" | "task_exit";
};

type BridgePendingState = {
  hasPendingRequests(): boolean;
};

type VersionMismatchPool = {
  replaceBinary(path: string): Promise<string>;
};

function createVersionMismatchHandler(
  getPool: () => VersionMismatchPool | undefined,
  ensureCompatibleBinary: (version?: string) => Promise<string | null> = ensureBinary,
) {
  // Coordinate concurrent version mismatches so followers wait for the first
  // download/hot-swap for the target plugin version instead of failing while
  // the compatible binary is still in flight.
  const versionUpgradePromises = new Map<string, Promise<string | null>>();

  return async (binaryVersion: string, minVersion: string): Promise<string | null> => {
    const existing = versionUpgradePromises.get(minVersion);
    if (existing) {
      log(`Version ${binaryVersion} < ${minVersion}; awaiting in-flight compatible binary upgrade`);
      return existing;
    }

    const upgradePromise = (async () => {
      warn(
        `WARNING: aft binary v${binaryVersion} is older than plugin v${minVersion}. ` +
          "Some features may not work. Attempting to download a compatible binary...",
      );
      try {
        const path = await ensureCompatibleBinary(`v${minVersion}`);
        if (!path) {
          warn(`Could not find or download v${minVersion}. Continuing with v${binaryVersion}.`);
          return null;
        }
        const pool = getPool();
        if (!pool) {
          warn(`Found/downloaded compatible binary at ${path}, but bridge pool is not ready.`);
          return null;
        }
        log(`Found/downloaded compatible binary at ${path}. Replacing running bridges...`);
        const replaced = await pool.replaceBinary(path);
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
        versionUpgradePromises.delete(minVersion);
      }
    })();
    versionUpgradePromises.set(minVersion, upgradePromise);
    return upgradePromise;
  };
}

/** Plugin version from package.json. */
const PLUGIN_VERSION: string = (() => {
  try {
    const req = createRequire(import.meta.url);
    return (req("../package.json") as { version: string }).version;
  } catch {
    return "0.0.0";
  }
})();

const ANNOUNCEMENT_VERSION = "0.58.0";
const ANNOUNCEMENT_FEATURES: string[] = [
  "Every AFT tool and background index, including semantic search, is on by default; `disabled_tools` is the one switch, and `npx @cortexkit/aft setup` walks you through the choices.",
  "Old config keys still work with a notice until 0.59; run `npx @cortexkit/aft doctor --fix` to migrate them.",
  "ONNX Runtime auto-install works again for new installs, and a config AFT cannot use now shows its error and fix instead of silently stopping the plugin.",
];

/**
 * Persistent footer rendered below the version-specific bullets in every
 * announcement. Stays in place across releases so users always see the Discord
 * invite without us needing to repeat it in `ANNOUNCEMENT_FEATURES` each time.
 *
 * Leave empty (`""`) to suppress.
 */
const ANNOUNCEMENT_FOOTER = "Join us on Discord: https://discord.gg/DSa65w8wuf";

const pendingEagerWarnings = new Map<string, ConfigureWarning[]>();

function isConfigureWarning(value: unknown): value is ConfigureWarning {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const warning = value as Record<string, unknown>;
  return (
    (warning.kind === "formatter_not_installed" ||
      warning.kind === "checker_not_installed" ||
      warning.kind === "lsp_binary_missing" ||
      warning.kind === "config_parse_failed" ||
      warning.kind === "config_key_dropped") &&
    typeof warning.hint === "string"
  );
}

function coerceConfigureWarnings(warnings: unknown[]): ConfigureWarning[] {
  return warnings.filter(isConfigureWarning);
}

type DroppedConfigKey = { key: string; tier: string; reason: string };

function isDroppedConfigKey(value: unknown): value is DroppedConfigKey {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const dropped = value as Record<string, unknown>;
  return (
    typeof dropped.key === "string" &&
    typeof dropped.tier === "string" &&
    typeof dropped.reason === "string"
  );
}

function coerceDroppedKeyWarnings(droppedKeys: unknown): ConfigureWarning[] {
  if (!Array.isArray(droppedKeys)) return [];
  return formatDroppedKeyWarnings(droppedKeys.filter(isDroppedConfigKey)).map((hint) => ({
    kind: "config_key_dropped" as const,
    hint,
  }));
}

function drainPendingEagerWarnings(projectRoot: string): ConfigureWarning[] {
  const pending = pendingEagerWarnings.get(projectRoot) ?? [];
  pendingEagerWarnings.delete(projectRoot);
  return pending;
}

function enqueueConfigParseWarnings(
  projectRoot: string,
  errors: ReturnType<typeof getConfigLoadErrors>,
): void {
  if (!projectRoot || errors.length === 0) return;
  const pending = pendingEagerWarnings.get(projectRoot) ?? [];
  for (const entry of errors) {
    const hint = formatConfigParseFailureMessage(entry.path, entry.message);
    if (!pending.some((item) => item.kind === "config_parse_failed" && item.hint === hint)) {
      pending.push({ kind: "config_parse_failed", hint });
    }
  }
  pendingEagerWarnings.set(projectRoot, pending);
}

function shouldPrepareOnnxRuntime(config: Pick<AftConfig, "indexes" | "semantic">): boolean {
  const isFastembedSemanticBackend = (config.semantic?.backend ?? "fastembed") === "fastembed";
  return resolveIndexes(config.indexes).semantic && isFastembedSemanticBackend;
}

function bridgeDirectoryFromCallback(bridge: unknown, fallback: string): string {
  const cwd = (bridge as { cwd?: unknown } | undefined)?.cwd;
  return typeof cwd === "string" && cwd.length > 0 ? cwd : fallback;
}

/** Longest the eager warmup waits for the ONNX Runtime before spawning without it. */
const ONNX_WARMUP_WAIT_CAP_MS = 60_000;

/**
 * Wait for `promise`, but give up after `capMs`. The cap timer is cleared as
 * soon as the promise settles and is unref'd while pending: it only bounds the
 * wait and must never be the thing that keeps the host process alive. A plain
 * `setTimeout` race here held every headless `pi -p` run open for the full
 * minute after it had already answered and shut the bridge pool down.
 */
async function waitWithCap<T>(promise: Promise<T>, capMs: number): Promise<T | null> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const cap = new Promise<null>((resolve) => {
    timer = setTimeout(() => resolve(null), capMs);
    (timer as { unref?: () => void }).unref?.();
  });
  try {
    return await Promise.race([promise, cap]);
  } finally {
    clearTimeout(timer);
  }
}

// IMPORTANT: NOT exported as a named export — only via the __test__
// namespace at the bottom. Pi's extension loader is different from
// OpenCode's, but OpenCode's plugin loader walks every top-level
// function export and treats them all as plugin entrypoints, which
// crashed our OpenCode-side plugin. Keeping both packages' surface
// shape identical avoids cross-contamination if shared utilities ever
// move between them.
async function handleConfigureWarningsForSession(context: {
  projectRoot: string;
  sessionId?: string | null;
  client?: unknown;
  bridge: Pick<import("@cortexkit/aft-bridge").AftProjectTransport, "send">;
  warnings: unknown[];
  configDroppedKeys?: unknown;
  storageDir: string;
  pluginVersion: string;
}): Promise<void> {
  const validWarnings = [
    ...coerceConfigureWarnings(context.warnings),
    ...coerceDroppedKeyWarnings(context.configDroppedKeys),
  ];

  if (!context.sessionId) {
    if (validWarnings.length === 0) return;
    const pending = pendingEagerWarnings.get(context.projectRoot) ?? [];
    pending.push(...validWarnings);
    pendingEagerWarnings.set(context.projectRoot, pending);
    warn(
      `[configure] deferred warnings for ${context.projectRoot} arrived without session_id; buffering until first session-bound call`,
    );
    return;
  }
  if (!context.client) {
    warn(
      `[configure] deferred warnings for session ${context.sessionId} arrived without notification client; skipping notification`,
    );
    return;
  }
  const pendingWarnings = drainPendingEagerWarnings(context.projectRoot);
  const combinedWarnings = [...pendingWarnings, ...validWarnings];
  if (combinedWarnings.length === 0) return;
  await deliverConfigureWarnings(
    {
      client: context.client,
      sessionId: context.sessionId,
      bridge: context.bridge,
      storageDir: context.storageDir,
      pluginVersion: context.pluginVersion,
      projectRoot: context.projectRoot,
    },
    combinedWarnings,
  );
}

/**
 * Pi extension default export.
 *
 * Called once per session. Registers tools, commands, and session shutdown hooks.
 */
export default async function (pi: ExtensionAPI): Promise<void> {
  recordActiveExtensionApi(pi);
  const deliverConfigMigrationWarnings = (messages: readonly string[]) => {
    for (const message of messages) {
      const notify = (pi as { ui?: { notify?: (message: string, type?: "warning") => void } }).ui
        ?.notify;
      if (typeof notify === "function") {
        try {
          notify(message, "warning");
          continue;
        } catch (err) {
          warn(`[config] failed to deliver migration notification: ${err}`);
        }
      }
      try {
        process.stderr.write(`
[AFT] ${message}
`);
      } catch {
        // stderr may be unavailable in embedded hosts; warn() still records it.
      }
    }
  };

  const projectRoot = process.cwd();
  // Load the AFT config before any binary, storage or index work. An unusable
  // configuration (a retired key after its migration window, an already
  // retired GitHub alias, a file that does not parse, a missing subc
  // connection file) still loads the extension, in the config error state:
  // the tools register but every call fails with the error and its fix.
  const bootstrap = await resolvePiBootstrapConfig(projectRoot, (message) =>
    deliverConfigMigrationWarnings([message]),
  );
  if (!bootstrap.ok) {
    registerPiConfigErrorState(pi, bootstrap.config, bootstrap.message);
    return;
  }
  const config = bootstrap.config;
  enqueueConfigParseWarnings(projectRoot, getConfigLoadErrors());
  deliverConfigLoadNotices((message) => deliverConfigMigrationWarnings([message]));
  const unknownDisabled = unknownDisabledTools(config.disabled_tools ?? []);
  if (unknownDisabled.length > 0) {
    // One aggregated notice per load; unknown names stay inert and preserved.
    deliverConfigMigrationWarnings([
      `unknown_disabled_tools: disabled_tools lists names AFT does not know: ${unknownDisabled.join(", ")}`,
    ]);
  }

  log(`AFT extension loading (plugin v${PLUGIN_VERSION})`);

  // Never execute a binary on the host thread. `findBinarySync` only accepts
  // binaries whose identity is known without running them (a versioned-cache
  // entry with a matching identity sidecar, or the npm platform package by its
  // manifest version). When it misses, a download starts in the background
  // while `findBinary` checks the remaining candidates on a worker thread. The
  // resolver and first-tool-call path share ensureBinary's in-process promise;
  // its filesystem lock also coordinates a second Pi/OpenCode process without
  // duplicate fetches. An explicit AFT_BINARY_PATH goes straight to
  // `findBinary`, which verifies its version off-thread.
  //
  // With a subc connection file the daemon runs the binary and the plugin never
  // spawns one (the transport factory fails loud instead of falling back to a
  // standalone bridge), so no local binary is resolved at all.
  const usesSubc = Boolean(config.subc?.connection_file?.trim());
  const explicitBinary = Boolean(process.env.AFT_BINARY_PATH?.trim());
  const trustedBinaryPath = usesSubc || explicitBinary ? null : findBinarySync(PLUGIN_VERSION);
  if (!usesSubc && !explicitBinary && !trustedBinaryPath) {
    void ensureBinary(PLUGIN_VERSION).then(
      (path) => {
        if (path) log(`Background binary warmup ready at ${path}`);
      },
      (err) => {
        warn(
          `Background binary warmup failed: ${err instanceof Error ? err.message : String(err)}`,
        );
      },
    );
  }

  // Resolve the AFT binary. On first run this downloads the platform binary to
  // ~/.cache/aft/bin/vX.Y.Z/aft; failures are reported through Pi's plugin loader.
  let binaryPath: string | null = null;
  if (!usesSubc) {
    try {
      binaryPath = trustedBinaryPath ?? (await findBinary(PLUGIN_VERSION));
    } catch (err) {
      warn(
        `Failed to resolve AFT binary: ${err instanceof Error ? err.message : String(err)}. ` +
          "Tools will not be registered.",
      );
      return;
    }
  }

  await ensureStorageMigrated({
    harness: "pi",
    binaryPath: binaryPath ?? undefined,
    logger: bridgeLogger,
  });

  const storageDir = resolveCortexKitStorageRoot();

  // ONNX runtime for semantic search (optional, best-effort).
  //
  // We deliberately do NOT block plugin load on this. The ONNX runtime archive
  // is 60–80 MB and on a slow connection this can take 30–120 seconds.
  // Awaiting it inline used to make Pi appear to hang during plugin load, and
  // SIGKILL'ing the host mid-download left partial state on disk that the
  // next launch had to recover from.
  //
  // Instead: kick off the download as a background promise and patch
  // `_ort_dylib_dir` into the pool's configure overrides as soon as it
  // settles. Bridges spawned AFTER the download finishes pick it up
  // automatically. `ensureOnnxRuntime` returns null on unsupported platforms.
  let onnxRuntimePromise: Promise<string | null> | null = null;
  // Which sessions skip, and why a plain headless run does not, is decided in
  // session-kind.ts next to the worker signal bash and bash_watch use.
  const skipEagerStartup = skipsEagerStartup();
  if (skipEagerStartup) {
    log(
      `${MAGIC_CONTEXT_SUBAGENT_ENV}=1: running as a pi-magic-context subagent, so skipping eager warmup, ONNX Runtime preparation and LSP auto-install; the bridge starts on the first AFT tool call`,
    );
  }
  if (!skipEagerStartup && shouldPrepareOnnxRuntime(config)) {
    onnxRuntimePromise = ensureOnnxRuntime(storageDir).catch((err) => {
      warn(`Failed to prepare ONNX Runtime: ${err instanceof Error ? err.message : String(err)}`);
      return null;
    });
  }

  // Build configure-time params forwarded to every bridge on spawn.
  // Core-domain config flows only through raw tiers; Rust owns merge +
  // trust-boundary stripping. Flat params below are plugin-computed process
  // state and must not be derived from aft.jsonc.
  const configOverrides = buildConfigTierConfigureParams(projectRoot, {
    storage_dir: storageDir,
  });
  let lspInstallCompletion: Promise<string[] | null> | null = null;
  // Set once the host's session_shutdown (or a process signal) begins tearing
  // the pool down. Startup work that is still pending at that point must not
  // call into the pool afterwards: the pool revives on demand, and a bridge
  // spawned after shutdown has no owner left to stop it, so a headless
  // `pi -p` run would never exit.
  let hostShutdownStarted = false;
  // _ort_dylib_dir is patched in asynchronously below once ensureOnnxRuntime
  // settles. Bridges spawned before that resolution don't get ORT and
  // semantic search returns "still building" until they restart.

  // ─────────────────────────── LSP auto-install ───────────────────────────
  // Mirrors the OpenCode plugin: discover relevant LSPs, surface cached bin
  // dirs to Rust as `lsp_paths_extra`, kick off background installs for
  // anything missing. The 7-day grace defends against newly-published
  // malicious versions. Best-effort — failures never block plugin startup.
  let lspAutoInstallPassLease: AutoInstallPassLease | null = null;
  try {
    const lspAutoInstall = !skipEagerStartup && (config.lsp?.auto_install ?? true);
    const lspGraceDays = config.lsp?.grace_days ?? 7;
    const lspVersions = config.lsp?.versions ?? {};
    const lspDisabled = new Set(config.lsp?.disabled ?? []);
    lspAutoInstallPassLease = lspAutoInstall ? claimLspAutoInstallPass() : null;
    const skippedByRecentAutoInstall = lspAutoInstall && lspAutoInstallPassLease === null;
    if (skippedByRecentAutoInstall) {
      log("[lsp] skipping auto-install (another instance ran one recently)");
    }
    const runSharedAutoInstall = lspAutoInstall && !skippedByRecentAutoInstall;
    // When `lsp.auto_install: false`, leave the list empty so the Rust-side
    // `detect_missing_lsp_binaries` loop in configure.rs skips its built-in
    // server walk entirely. Without this gate, users who opted out of
    // auto-install still received `lsp_binary_missing` toasts/log warnings
    // on every configure. Explicit `lsp.servers` entries are unaffected.
    configOverrides.lsp_auto_install_binaries = lspAutoInstall
      ? [...new Set([...NPM_LSP_TABLE, ...GITHUB_LSP_TABLE].map((spec) => spec.binary))]
      : [];

    const npmResult = runAutoInstall(projectRoot, {
      autoInstall: runSharedAutoInstall,
      graceDays: lspGraceDays,
      versions: lspVersions,
      disabled: lspDisabled,
    });
    // The relevance scan only feeds install decisions, so skip the walk when
    // no install can start.
    const relevantGithub = runSharedAutoInstall
      ? discoverRelevantGithubServers(projectRoot)
      : new Set<string>();
    const ghResult = runGithubAutoInstall(relevantGithub, {
      autoInstall: runSharedAutoInstall,
      graceDays: lspGraceDays,
      versions: lspVersions,
      disabled: lspDisabled,
    });
    const mergedBinDirs = [...npmResult.cachedBinDirs, ...ghResult.cachedBinDirs];
    if (mergedBinDirs.length > 0) {
      configOverrides.lsp_paths_extra = mergedBinDirs;
    }
    const lspInflightInstalls = [
      ...new Set([...npmResult.installingBinaries, ...ghResult.installingBinaries]),
    ];
    if (lspInflightInstalls.length > 0) {
      configOverrides.lsp_inflight_installs = lspInflightInstalls;
    }
    const installsWereStarted = npmResult.installsStarted > 0 || ghResult.installsStarted > 0;
    if (installsWereStarted) {
      log(
        `[lsp] auto-install: ${npmResult.installsStarted} npm + ${ghResult.installsStarted} github install(s) running in background`,
      );
    }

    // ─── Surface install outcomes once installs settle ───
    //
    // Pi loads this extension once at startup, before any session exists, so
    // we can't send an ignored session message the way the OpenCode plugin
    // does. Instead we promote actionable skips from `log()` (verbose) to
    // `warn()` (visible at WARN level) so users running with default logging
    // see them. Routine skips (already-installed, not-relevant, disabled)
    // stay out of the warning summary.
    const installCompletion = Promise.all([npmResult.installsComplete, ghResult.installsComplete])
      .then(() => {
        if (installsWereStarted || skippedByRecentAutoInstall) {
          const updatedPaths = [
            ...new Set([...npmResult.getCachedBinDirs(), ...ghResult.getCachedBinDirs()]),
          ];
          if (updatedPaths.length > 0) {
            configOverrides.lsp_paths_extra = updatedPaths;
          } else {
            delete configOverrides.lsp_paths_extra;
          }
          return updatedPaths;
        }
        return null;
      })
      .then((updatedPaths) => {
        const actionable = [...npmResult.skipped, ...ghResult.skipped].filter((s) => {
          const r = s.reason.toLowerCase();
          if (r === "auto_install: false") return false;
          if (r === "disabled by config") return false;
          if (r === "not relevant to project") return false;
          if (r === "already installed") return false;
          if (r === "another install in progress") return false;
          return true;
        });
        if (actionable.length > 0) {
          const lines = actionable.map((s) => `  • ${s.id}: ${s.reason}`).join("\n");
          warn(
            `[lsp] skipped or failed to install ${actionable.length} server(s):\n${lines}\n` +
              'Pin a working version with `lsp.versions: { "<package>": "<version>" }` if grace is blocking, ' +
              "or set `lsp.auto_install: false` to suppress.",
          );
        }
        return updatedPaths;
      })
      .catch((err) => {
        warn(`[lsp] install-summary aggregation failed: ${err}`);
        return null;
      })
      .finally(() => {
        lspAutoInstallPassLease?.release();
        lspAutoInstallPassLease = null;
      });
    if (installsWereStarted || skippedByRecentAutoInstall) {
      lspInstallCompletion = installCompletion;
    }
  } catch (err) {
    lspAutoInstallPassLease?.release();
    lspAutoInstallPassLease = null;
    warn(`[lsp] auto-install setup failed: ${err instanceof Error ? err.message : String(err)}`);
  }

  let pool: AftTransportPool;
  const poolOptions: import("@cortexkit/aft-bridge").PoolOptions & {
    onBashLongRunning: (reminder: BashLongRunningPayload, bridge: BridgePendingState) => void;
    onBashPatternMatch: (frame: BashPatternMatchPayload, bridge: BridgePendingState) => void;
  } = {
    ...resolveBridgePoolTransportOptions(config),
    errorPrefix: "[aft-pi]",
    minVersion: PLUGIN_VERSION,
    onVersionMismatch: createVersionMismatchHandler(() => pool),
    onConfigureWarnings: ({ projectRoot, sessionId, client, warnings, configDroppedKeys }) => {
      const bridge = pool.getActiveBridgeForRoot(projectRoot);
      if (!bridge) return;
      const pendingWarnings = sessionId ? drainPendingEagerWarnings(projectRoot) : [];
      // Avoid re-entering bridge.send() from the synchronous configure callback
      // before aft-bridge marks the lazy-spawned bridge configured.
      setTimeout(() => {
        void handleConfigureWarningsForSession({
          projectRoot,
          sessionId,
          client,
          bridge,
          warnings: [...pendingWarnings, ...warnings],
          configDroppedKeys,
          storageDir,
          pluginVersion: PLUGIN_VERSION,
        });
      }, 0);
    },
    onBashCompletion: (completion, bridge) => {
      const directory = bridgeDirectoryFromCallback(bridge, process.cwd());
      void handlePushedBgCompletion(
        {
          ctx,
          directory,
          sessionID: completion.session_id,
          runtime: pi,
        },
        completion,
      );
    },
    onBashLongRunning: (reminder, bridge) => {
      const directory = bridgeDirectoryFromCallback(bridge, process.cwd());
      void handlePushedBgLongRunning(
        {
          ctx,
          directory,
          sessionID: reminder.session_id,
          runtime: pi,
        },
        reminder,
      );
    },
    onBashPatternMatch: (frame, bridge) => {
      const directory = bridgeDirectoryFromCallback(bridge, process.cwd());
      const liveSession = getActiveSessionId();
      void handlePushedPatternMatch(
        {
          ctx,
          directory,
          sessionID: liveSession ?? frame.session_id,
          runtime: pi,
        },
        frame,
      );
    },
  };
  // SINGLE transport injection point (B-FINAL S4): standalone NDJSON bridge
  // (default) OR the subc daemon, selected by the USER-tier subc.connection_file.
  // Fails loud if subc is selected but its connection file is absent.
  pool = await createAftTransportPool({
    harness: "pi",
    binaryPath,
    poolOptions,
    configOverrides,
    subcConnectionFile: config.subc?.connection_file,
    // Reaping-disabled pools have no root generation, so their nudges cannot carry
    // BgNudgeRef provenance. Keep the root/session callback wired as the delivery
    // path for those pools; bg-notifications coalesces it with provenance callbacks.
    onBgEventsNudge: (directory, sessionID) => {
      void handleSubcBgEventsNudge({
        ctx,
        directory,
        sessionID,
        runtime: pi,
      }).catch((err) => {
        warn(`[aft-pi] bg nudge rejected: ${err instanceof Error ? err.message : String(err)}`);
      });
    },
    onBgEventsNudgeRef: (ref) => {
      void handleSubcBgEventsNudge({
        ctx,
        directory: ref.canonicalRoot,
        sessionID: ref.session,
        nudgeRef: ref,
        runtime: pi,
      }).catch((err) => {
        warn(`[aft-pi] bg nudge rejected: ${err instanceof Error ? err.message : String(err)}`);
      });
    },
  });
  if (lspInstallCompletion) {
    lspInstallCompletion.then((updatedPaths) => {
      if (!updatedPaths) return;
      void pushLspPathsAfterAutoInstall(pool, projectRoot, updatedPaths)
        .then(() => {
          log(
            `[lsp] lsp_paths_extra updated after auto-install: ${updatedPaths.length} dirs pushed to live bridges`,
          );
        })
        .catch((err) => {
          warn(`[lsp] live bridge lsp_paths_extra update failed: ${err}`);
        });
    });
  }
  pool.setConfigureOverride("harness", "pi");
  const surface = resolvePiToolSurface(config, pi);
  // Hashline needs the tagged read slot as well as the edit slot: without it
  // nothing in the session can mint the tags a patch addresses, so the carrier
  // must report the surface as unusable rather than half-enabled.
  const hashlineEditRegistered = piHashlineEffective(config, surface);
  const hashlineDowngrade = piHashlineDowngrade(config, surface);
  if (hashlineDowngrade) {
    // Warn once per load when hashline editing is requested but `read` or `edit`
    // is disabled; the tools that are still registered keep their normal behavior.
    warn(`[hashline] ${hashlineDowngrade.code}: ${hashlineDowngrade.message}`);
    deliverConfigMigrationWarnings([hashlineDowngrade.message]);
  }
  pool.setConfigureOverride("edit_slot_survives", hashlineEditRegistered);
  // Tell Rust whether `aft_search` is registered for this surface so the
  // grep-rewrite footer steers there (vs the grep tool). Set before the eager
  // warmup spawn below so even the first bridge configures with the flag.
  // `.semantic` is the registration predicate: aft_search is not disabled.
  pool.setConfigureOverride("aft_search_registered", surface.semantic);
  const ctx: PluginContext = {
    pool,
    config,
    hashlineEffective: hashlineEditRegistered,
    storageDir,
  };

  // Settle the ONNX runtime download promise (started above) and patch the
  // resolved path into the pool's configure overrides. Bridges spawned AFTER
  // this resolves will pass `_ort_dylib_dir` through configure and pick up
  // the runtime; bridges already running at resolution time keep going
  // without ORT (we don't restart them — that would discard warm
  // trigram/semantic/LSP state). Result: semantic search becomes available
  // for new sessions automatically once the download completes.
  if (onnxRuntimePromise) {
    onnxRuntimePromise.then(
      (ortDylibDir) => {
        if (ortDylibDir) {
          pool.setConfigureOverride("_ort_dylib_dir", ortDylibDir);
          log(`ONNX Runtime ready at ${ortDylibDir}; new bridges will load semantic backend.`);
        } else {
          const reason = getOnnxRuntimeInstallFailure();
          if (reason) {
            // The managed install failed. Say so in the UI, not only the log.
            warn(`ONNX Runtime unavailable: ${reason}. Semantic search will be disabled.`);
            deliverConfigMigrationWarnings([
              `Semantic search is unavailable: ONNX Runtime could not be installed (${reason}).\nRetry with: npx @cortexkit/aft doctor --fix`,
            ]);
          } else {
            warn(
              `ONNX Runtime unavailable. Semantic search will be disabled. Install manually: ${getManualInstallHint()}`,
            );
          }
        }
      },
      (err) => {
        warn(`ONNX Runtime resolution rejected unexpectedly: ${err}`);
      },
    );
  }

  // Eager async configure: warm the bridge for `process.cwd()` so the first
  // tool call doesn't pay the spawn + configure latency. Errors are swallowed —
  // the next real tool call will surface a proper error.
  //
  // AUDIT NOTE (intentional — do not flag as a bug): unlike the OpenCode
  // plugin, Pi keeps eager warmup ON PURPOSE. Pi runs one bridge per process
  // (one session per process) and has no OpenCode-Desktop-style sidebar that
  // multiplies plugin instances across many projects/worktrees. The reason
  // OpenCode went lazy (avoid spawning N bridges for N sidebar projects at
  // startup) does not apply here, so eager warmup is the correct trade for Pi:
  // it removes first-tool-call latency without the bridge-storm downside.
  // The $HOME guard below is the only case we skip. See the home-dir note.
  // (pi-magic-context subagents also skip it; see skipsEagerStartup.)
  void (async () => {
    try {
      if (skipEagerStartup) return;
      // Note #65: skip eager configure when Pi was launched from the user's
      // home directory. Configuring on `$HOME` walks the entire user home
      // tree (100k–10M files), times out the 30s configure budget, gets
      // killed, then silently retries on every reload. The first real tool
      // call from a session will still warm the correct project bridge.
      const cwd = process.cwd();
      if (isHomeDirectoryRoot(canonicalizeProjectRoot(cwd))) {
        log(
          `Eager configure skipped: cwd=${cwd} is the user home directory. ` +
            `The first real tool call will warm the correct project bridge.`,
        );
        return;
      }
      // Await ONNX runtime resolution BEFORE spawning the bridge — otherwise
      // the bridge starts without _ort_dylib_dir on its configure overrides,
      // and Rust falls back to a system-path dlopen("libonnxruntime.dylib")
      // that almost always fails on macOS / Windows (the user has the runtime
      // managed in `<storage_dir>/onnxruntime/`, not on the system loader
      // path). The race symptom in the wild: log shows
      //   Spawning binary: ...
      //   ONNX Runtime ready at ...   <- 4ms later
      //   failed to build semantic index: ONNX Runtime not found
      // because the bridge spawn at t=0 has no _ort_dylib_dir yet, and once
      // it's set on the pool only NEW bridges pick it up. Mirror the
      // OpenCode plugin: cap at 60s so a slow/broken download doesn't block
      // the warmup permanently; the bridge still spawns without ORT after
      // the cap and semantic just fails honestly.
      if (onnxRuntimePromise) {
        await waitWithCap(onnxRuntimePromise, ONNX_WARMUP_WAIT_CAP_MS);
      }
      if (hostShutdownStarted) {
        log("Eager configure skipped: the host shut the session down before warmup ran.");
        return;
      }
      const bridge = pool.getBridge(cwd);
      // No session_id: runs before any user session exists; configure
      // threads spawned by this warmup will log with no [ses_xxx] prefix.
      const response = await bridge.send("status", {});
      // Seed the plugin-side cache so the /aft-status overlay's first poll
      // after spawn finds a warm snapshot instead of racing into bridge.send
      // and hitting the client timeout while the bridge dispatch loop is
      // still finishing configure. Push frames will overwrite this with
      // fresh data on every state transition (1s debounce).
      if (response.success !== false) {
        bridge.cacheStatusSnapshot(response as Parameters<typeof bridge.cacheStatusSnapshot>[0]);
      }
    } catch (err) {
      log(`eager configure failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  })();

  if (ANNOUNCEMENT_VERSION && ANNOUNCEMENT_FEATURES.length > 0) {
    sendFeatureAnnouncement(
      ANNOUNCEMENT_VERSION,
      ANNOUNCEMENT_FEATURES,
      ANNOUNCEMENT_FOOTER,
      storageDir,
    );
  }

  registerPiToolSurface(pi, ctx, surface);

  // Pi binds its live tool registry after extension factories run. A modern Pi
  // can therefore reveal that its optional built-in PowerShell tool is active
  // only at session start; older hosts use bash.powershell_tool instead.
  let powershellRegistered = surface.hoistPowershell;
  (
    pi.on as (
      event: "session_start",
      handler: (_event?: unknown, extCtx?: unknown) => unknown,
    ) => void
  )("session_start", (_event, extCtx) => {
    const sessionID = extCtx ? resolveSessionId(extCtx as ExtensionContext) : undefined;
    setActiveSessionId(sessionID);
    if (powershellRegistered) return;
    const liveSurface = resolvePiToolSurface(config, pi);
    if (!liveSurface.hoistPowershell) return;
    // Companions were registered by name in the first pass, independently of
    // which shell tools exist, so only the PowerShell slot is added here.
    registerBashTool(pi, ctx, liveSurface.semantic, "powershell", false, "powershell");
    powershellRegistered = true;
  });

  // Workflow hints: short system-prompt block teaching token-efficient
  // AFT workflows. Hooked into Pi's `before_agent_start` event with
  // systemPrompt extension. Always-on; conditional on the registered
  // tool surface so absent tools aren't advertised.
  registerWorkflowHints(pi, config, surface);

  // Slash command: /aft-status
  registerStatusCommand(pi, ctx);

  (
    pi.on as (
      event: "tool_result",
      handler: (
        event: {
          content: Array<
            { type: "text"; text: string } | { type: "image"; data: string; mimeType: string }
          >;
          details: unknown;
          isError: boolean;
        },
        ctx: Parameters<typeof resolveSessionId>[0] & { cwd: string },
      ) => unknown,
    ) => void
  )("tool_result", async (event, extCtx) => {
    const sessionID = resolveSessionId(extCtx);
    setActiveSessionId(sessionID);
    const bgContent = await appendToolResultBgCompletions(
      { ctx, directory: extCtx.cwd, sessionID },
      event.content,
    );
    // Start from the bg-completion-augmented content if present, else original.
    const content = bgContent ?? event.content;
    // Nothing to add → leave the tool result untouched.
    if (content === event.content) return undefined;
    return { content, details: event.details, isError: event.isError };
  });

  (
    pi.on as (
      event: "turn_end",
      handler: (
        event: unknown,
        ctx: Parameters<typeof resolveSessionId>[0] & { cwd: string },
      ) => unknown,
    ) => void
  )("turn_end", async (_event, extCtx) => {
    const sessionID = resolveSessionId(extCtx);
    setActiveSessionId(sessionID);
    await handleTurnEndBgCompletions({
      ctx,
      directory: extCtx.cwd,
      sessionID,
      runtime: pi,
    });
    const bridge = pool.getActiveBridgeForRoot(extCtx.cwd);
    if (bridge && sessionID) {
      void handleConfigureWarningsForSession({
        projectRoot: extCtx.cwd,
        sessionId: sessionID,
        client: pi,
        bridge,
        warnings: [],
        storageDir,
        pluginVersion: PLUGIN_VERSION,
      });
    }
  });

  // User-message abort: when the user sends a message while the agent is
  // blocked in a sync bash_watch wait or a wait:true foreground bash, signal
  // the wait to detach so the user is not locked out.
  (
    pi.on as (
      event: "input",
      handler: (
        event: { type: string; text: string; source: string },
        ctx: Parameters<typeof resolveSessionId>[0] & { cwd: string },
      ) => unknown,
    ) => void
  )("input", (event, extCtx) => {
    const sessionId = resolveSessionId(extCtx);
    setActiveSessionId(sessionId);
    signalSyncWatchAbort(sessionId);
    const originalText = event.text;
    const transformedText = stripUserMessageDetachKeyword(originalText);
    const shouldDetach = shouldDetachBashWaitOnUserMessage(config, originalText);
    if (shouldDetach) {
      void signalBashWaitDetachForProject(pool, extCtx.cwd, sessionId);
    }
    if (transformedText !== originalText) {
      return { action: "transform", text: transformedText };
    }
  });

  // Also register process-level signal handlers so children get an orderly
  // shutdown when Pi's host Node process is killed directly (terminal close,
  // Ctrl+C, OS shutdown) rather than through the session_shutdown lifecycle.
  const unregisterShutdownCleanup = registerShutdownCleanup(async () => {
    hostShutdownStarted = true;
    try {
      await Promise.allSettled([abortInFlightAutoInstalls(), abortInFlightGithubInstalls()]);
      await pool.shutdown();
    } catch (err) {
      warn(`Error during process shutdown: ${err instanceof Error ? err.message : String(err)}`);
    }
    await flushLogs();
  });

  // Clean up bridges on session shutdown.
  pi.on("session_shutdown", async () => {
    hostShutdownStarted = true;
    try {
      await Promise.allSettled([abortInFlightAutoInstalls(), abortInFlightGithubInstalls()]);
      await pool.shutdown();
      log("Bridge pool shut down");
    } catch (err) {
      warn(`Error during bridge shutdown: ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      unregisterShutdownCleanup();
      await flushLogs();
    }
  });

  log(`AFT extension ready (disabled_tools=${(config.disabled_tools ?? []).join(",") || "none"})`);
}

export const __test__ = {
  enqueueConfigParseWarnings,
  bridgeDirectoryFromCallback,
  resolveToolSurface: resolvePiToolSurface,
  handleConfigureWarningsForSession,
  shouldPrepareOnnxRuntime,
  createVersionMismatchHandler,
};
