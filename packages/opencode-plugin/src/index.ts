import {
  createAftTransportPool,
  ensureBinary,
  ensureOnnxRuntime,
  ensureStorageMigrated,
  findBinary,
  findBinarySync,
  getManualInstallHint,
  isOrtAutoDownloadSupported,
  markAnnouncementSeen,
  resolveCortexKitStorageRoot,
  setActiveLogger,
  shouldShowAnnouncement,
} from "@cortexkit/aft-bridge";
import type { Plugin } from "@opencode-ai/plugin";

import {
  extractUserMessageText,
  shouldDetachBashWaitOnUserMessage,
  signalBashWaitDetachForProject,
  stripUserMessageDetachKeyword,
} from "./bash-wait-detach.js";
import {
  appendInTurnBgCompletions,
  extractSessionID,
  handleIdleBgCompletions,
  handlePushedBgCompletion,
  handlePushedBgLongRunning,
  handlePushedPatternMatch,
  handleSubcBgEventsNudge,
} from "./bg-notifications.js";
import {
  buildConfigTierConfigureParams,
  getConfigLoadErrors,
  loadAftConfig,
  migrateAftConfigLocations,
  resolveBashConfig,
  resolveBridgePoolTransportOptions,
  resolveOpenCodeRegistrationRoot,
} from "./config.js";
import {
  drainPendingConfigParseWarnings,
  enqueueConfigParseWarnings,
  enqueueConfigureWarningsForSession,
  flushConfigureWarningsOnIdle,
} from "./configure-warnings.js";
import { createAutoUpdateCheckerHook } from "./hooks/auto-update-checker/index.js";
import { bridgeLogger, error, log, warn } from "./logger.js";
import {
  abortInFlightAutoInstalls,
  pushLspPathsAfterAutoInstall,
  runAutoInstall,
} from "./lsp-auto-install.js";
import {
  abortInFlightGithubInstalls,
  discoverRelevantGithubServers,
  runGithubAutoInstall,
} from "./lsp-github-install.js";
import { GITHUB_LSP_TABLE } from "./lsp-github-table.js";
import { NPM_LSP_TABLE } from "./lsp-npm-table.js";
import { prepareOpenCodeArguments } from "./normalize-schemas.js";
import {
  cleanupWarnings,
  type NotificationOptions,
  sendFeatureAnnouncement,
  sendWarning,
} from "./notifications.js";
import { resolvePluginVersion } from "./plugin-version.js";
import { maybeAppendConflictsHint } from "./shared/bash-hints.js";
import { sendIgnoredMessage } from "./shared/ignored-message.js";
import {
  drainNotifications,
  isTuiConnected,
  pushNotification,
  pushStatusChange,
} from "./shared/rpc-notifications.js";
import { AftRpcServer } from "./shared/rpc-server.js";
import {
  getSessionDirectory,
  getSessionDirectoryCached,
  verifySessionDirectory,
  warmSessionDirectory,
} from "./shared/session-directory.js";
import { coerceAftStatus, formatStatusMarkdown } from "./shared/status.js";
import { registerShutdownCleanup, runCleanups } from "./shutdown-hooks.js";
import { signalSyncWatchAbort } from "./sync-watch-abort.js";
import { instrumentToolMap } from "./tool-perf.js";
import {
  buildAftToolDefinitions,
  openCodeHashlineDowngrade,
  openCodeHashlineEditRegistered,
  openCodeHashlineEffective,
} from "./tool-registration.js";
import { bashToolDescription } from "./tools/bash.js";
import { createInspectTier2IdleScheduler } from "./tools/inspect.js";
import type { PluginContext } from "./types.js";
import { appendHintsToSystem, buildHintsFromConfig } from "./workflow-hints.js";

type BashPatternMatchPayload = {
  session_id: string;
  task_id: string;
  watch_id: string;
  match_text: string;
  match_offset: number;
  context: string;
  once: boolean;
};

type BashLongRunningPayload = {
  session_id: string;
  task_id: string;
  command: string;
  elapsed_ms: number;
  mode?: "pipes" | "pty" | string;
};

type BridgePendingState = {
  hasPendingRequests(): boolean;
  getCwd(): string;
};

// Register our logger with @cortexkit/aft-bridge before any bridge code runs.
// Module side-effect: import order matters because BridgePool / BinaryBridge
// internals call the active-logger helpers (log/warn/error) from constructors.
setActiveLogger(bridgeLogger);

const STATUS_COMMAND = "aft-status";
const SENTINEL_PREFIX = "__AFT_STATUS_";

// Effect HTTP plain-string TypeIds (NOT Symbols), verified against effect 4.x
// source. Because the guards are `key in obj` checks on string keys, a hand-built
// object carries them with NO effect import and NO version/realm coupling — it
// works on the compiled OpenCode binary where effect is unreachable from an
// external plugin's module resolution.
const HTTP_SERVER_RESPONSE_TYPE_ID = "~effect/http/HttpServerResponse";
const HTTP_COOKIES_TYPE_ID = "~effect/http/Cookies";
const HTTP_BODY_TYPE_ID = "~effect/http/HttpBody";
const ERROR_REPORTER_IGNORE = "~effect/ErrorReporter/ignore";

/** Prevent OpenCode from forwarding the handled command to the LLM.
 *
 *  We throw a normal `Error` whose message preserves the legacy sentinel text
 *  for hosts that do NOT recognize the Effect HTTP tags. The same Error also
 *  duck-types an Effect `HttpServerResponse.empty({ status: 204 })`. On
 *  OpenCode 1.17.x the HTTP error boundary recognizes it via the plain-string
 *  TypeId (`isHttpServerResponse` = `"~effect/http/HttpServerResponse" in defect`),
 *  skips the JSON-500 logging path, and writes it as a real 204 — so the handled
 *  command neither reaches the LLM nor leaks an error into the TUI/log.
 *
 *  Field shape is the minimal set `Response.toWeb` dereferences on the empty-body
 *  path (status / statusText / headers / cookies.cookies / body._tag), traced
 *  through effect 4.x. No effect import — all string keys + primitives.
 *
 *  An official `command.execute.before` handled/cancel/noReply contract remains
 *  the real fix; this is a duck-typed shim until then. */
function throwSentinel(command: string): never {
  const sentinel = new Error(
    `${SENTINEL_PREFIX}${command.toUpperCase().replace(/-/g, "_")}_HANDLED__`,
  ) as Error & Record<string, unknown>;
  sentinel[HTTP_SERVER_RESPONSE_TYPE_ID] = HTTP_SERVER_RESPONSE_TYPE_ID;
  Object.defineProperty(sentinel, ERROR_REPORTER_IGNORE, { value: true, enumerable: true });
  sentinel.status = 204;
  sentinel.statusText = undefined;
  sentinel.headers = {};
  sentinel.cookies = { [HTTP_COOKIES_TYPE_ID]: HTTP_COOKIES_TYPE_ID, cookies: {} };
  sentinel.body = { [HTTP_BODY_TYPE_ID]: HTTP_BODY_TYPE_ID, _tag: "Empty" };
  throw sentinel;
}

// IMPORTANT — index.ts must export ONLY the plugin function as default.
// OpenCode's plugin loader (`getLegacyPlugins` in
// `~/Work/OSS/opencode/packages/opencode/src/plugin/index.ts`) walks
// `Object.values(mod)` and rejects any non-function top-level export
// with `TypeError: Plugin export is not a function`. Function exports
// (other than the default plugin) get treated as additional plugin
// entrypoints, called with OpenCode's plugin input, and their return
// value pushed into the hooks array — `undefined` returns then crash
// the host on every `hook.config?.(cfg)` / `hook.provider?.(...)` /
// etc. iteration. Helpers stay in sibling modules.
// sendIgnoredMessage moved to ./shared/ignored-message.ts so tools/permissions.ts
// can call it too (index.ts must export only the plugin default).

/** The plugin's own version, resolved from whichever entry bundle this runs in. */
const PLUGIN_VERSION: string = resolvePluginVersion(import.meta.url);

/**
 * Release-notes identifier for the startup announcement dialog.
 *
 * This is intentionally decoupled from PLUGIN_VERSION so bugfix releases don't
 * re-trigger a stale dialog. Bump this string and populate ANNOUNCEMENT_FEATURES
 * ONLY when a release ships user-facing news worth surfacing once at startup.
 * Leave ANNOUNCEMENT_VERSION empty (or ANNOUNCEMENT_FEATURES empty) to skip the
 * dialog entirely for bugfix-only releases.
 *
 * Persistence (storage/last_announced_version) stores this value, so once a user
 * dismisses an announcement, patch releases that don't bump ANNOUNCEMENT_VERSION
 * will not re-show it.
 */
const ANNOUNCEMENT_VERSION = "0.55.1";
const ANNOUNCEMENT_FEATURES: string[] = [
  "Callgraph stays consistent under load: edits arriving mid-rebuild can no longer be silently lost from navigation results.",
  "aft_zoom handles real-world .jsonc: files with comment banners above the opening brace now resolve, and not-found errors point at the segment that actually failed (thanks @iceteaSA).",
  "Disk hygiene: stale per-checkout inspect caches from old worktrees are now swept automatically \u2014 one machine reclaimed 21 GB.",
];

/**
 * Persistent footer rendered below the version-specific bullets in every
 * announcement. Stays in place across releases so users always see the Discord
 * invite without us needing to repeat it in `ANNOUNCEMENT_FEATURES` each time.
 *
 * Leave empty (`""`) to suppress.
 */
const ANNOUNCEMENT_FOOTER = "Join us on Discord: https://discord.gg/DSa65w8wuf";

/**
 * AFT (Agent File Toolkit) plugin for OpenCode.
 *
 * Config is loaded from two levels (project overrides user):
 * - User:    ~/.config/opencode/aft.jsonc (or .json)
 * - Project: <project>/.opencode/aft.jsonc (or .json)
 *
 * Tools organized into groups:
 * - Hoisted (default): read, write, edit, apply_patch, ast_grep_search, ast_grep_replace
 *   and grep/glob when search_index is enabled
 * - File ops: aft_delete, aft_move
 * - Reading: aft_outline
 * - Safety: aft_safety
 * - Imports: aft_import
 * - Navigation: aft_callgraph
 */
// OpenCode currently calls this function more than once per process when a
// single plugin is configured — see https://github.com/anomalyco/opencode/issues/26812.
// The duplicate calls run in independent ESM module graphs with isolated
// `globalThis` / `process.env` / `Symbol.for` registries, so there is no
// in-process state we can use to dedupe from the plugin side. Earlier
// in-process dedup attempts (globalThis-keyed Map, see commit 05af89e) did
// not work and have been removed. The fix belongs upstream in OpenCode.
const plugin: Plugin = async (input) => initializePluginForDirectory(input);

async function initializePluginForDirectory(input: Parameters<Plugin>[0]) {
  const deliverConfigMigrationWarnings = (directory: string, messages: readonly string[]) => {
    for (const message of messages) {
      void sendWarning({ client: input.client, directory }, message).catch((err) => {
        warn(`[config] failed to deliver migration warning: ${err}`);
      });
    }
  };

  // OpenCode can initialize a plugin from a nested session directory while
  // registering one schema for the checkout. Use that checkout for project
  // config so schema selection and the bridge's runtime project agree.
  const registrationRoot = resolveOpenCodeRegistrationRoot(input.directory, input.worktree);

  // Load the AFT config before any binary or storage work. This ensures
  // `enabled: false` makes AFT do nothing except read the config file.
  let aftConfig = loadAftConfig(registrationRoot);
  if (aftConfig.enabled === false) {
    log(`AFT disabled by config for ${registrationRoot}`);
    return { tool: {} };
  }

  deliverConfigMigrationWarnings(
    registrationRoot,
    migrateAftConfigLocations(registrationRoot, bridgeLogger).flatMap((result) => result.warnings),
  );

  // Load config: ~/.config/cortexkit/aft.jsonc → <project>/.cortexkit/aft.jsonc
  aftConfig = loadAftConfig(registrationRoot);
  enqueueConfigParseWarnings(registrationRoot, getConfigLoadErrors());
  if (aftConfig.enabled === false) {
    log(`AFT disabled by config for ${registrationRoot}`);
    return { tool: {} };
  }

  // Probe synchronously so a missing or mismatched cache entry can start its
  // download before the rest of plugin startup does any work. The resolver and
  // first-tool-call path share ensureBinary's in-process promise; its filesystem
  // lock also coordinates a second OpenCode process without duplicate fetches.
  const cachedBinaryPath = findBinarySync(PLUGIN_VERSION);
  if (!cachedBinaryPath) {
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
  const binaryPath = cachedBinaryPath ?? (await findBinary(PLUGIN_VERSION));

  await ensureStorageMigrated({ harness: "opencode", binaryPath, logger: bridgeLogger });
  const autoUpdateAbort = new AbortController();
  const projectEnabledCache = new Map<string, boolean>([[registrationRoot, true]]);
  const loggedDisabledProjects = new Set<string>();
  const isProjectEnabled = (projectRoot: string): boolean => {
    const cached = projectEnabledCache.get(projectRoot);
    if (cached !== undefined) return cached;
    const projectConfig = loadAftConfig(projectRoot);
    const enabled = projectConfig.enabled !== false;
    projectEnabledCache.set(projectRoot, enabled);
    if (!enabled && !loggedDisabledProjects.has(projectRoot)) {
      loggedDisabledProjects.add(projectRoot);
      log(`AFT disabled by config for ${projectRoot}`);
    }
    return enabled;
  };

  // Build configure params for the Rust binary.
  //
  // **Two layers**:
  //   1. `configOverrides` (this block) — GLOBAL per-process state shared by
  //      every bridge: storage_dir, _ort_dylib_dir (patched later), harness,
  //      bash_permissions, lsp_paths_extra (LSP install cache), plus raw config
  //      tiers for the init-time project so the first bridge configures without
  //      an extra loader call.
  //   2. `projectConfigLoader` (wired below) — PER-BRIDGE raw config tiers,
  //      loaded from each project's own `.cortexkit/aft.jsonc` at bridge-spawn
  //      time. Rust owns merge + trust-boundary stripping for the core domain.
  //
  // The pool merges them with per-project values winning. Without this split,
  // OpenCode Desktop / `opencode serve` (one plugin instance, many projects)
  // would burn the wrong project's config into every bridge — the project
  // visible at plugin init time would override everything.
  //
  // Core-domain config now flows only through `config: [{ tier, source, doc }]`.
  // Do not add resolved aft.jsonc fields (format_on_edit, semantic, lsp, etc.)
  // to this flat map; flat params below are plugin-computed process state.
  const storageDir = resolveCortexKitStorageRoot();
  const configOverrides: Record<string, unknown> = buildConfigTierConfigureParams(
    registrationRoot,
    {
      bash_permissions: true,
      storage_dir: storageDir,
    },
  );
  // Build tool schemas from the requested hashline setting. After disabled-tool
  // filtering, the code below freezes whether the final edit tool supports hashline.
  const hashlineEffective = openCodeHashlineEffective(aftConfig);
  let lspInstallCompletion: Promise<string[] | null> | null = null;

  const isFastembedSemanticBackend = (aftConfig.semantic?.backend ?? "fastembed") === "fastembed";

  // v0.27 stores runtime state under the shared CortexKit root. Migration from
  // the legacy OpenCode plugin root completed synchronously before any storage
  // consumer (ONNX, RPC server, bridge configure) can touch this path.

  // Auto-resolve ONNX Runtime for semantic search.
  //
  // We deliberately do NOT block plugin load on this. The ONNX runtime archive
  // is 60–80 MB and on a slow connection this can take 30–120 seconds. Awaiting
  // it inline used to make OpenCode appear to hang ("blackscreen on launch")
  // until the download finished, and SIGKILL'ing the host mid-download left
  // partial state that the next launch had to recover from.
  //
  // Instead: kick off the download as a background promise, let the plugin
  // finish registering tools immediately, and patch `_ort_dylib_dir` into the
  // pool's configure overrides as soon as the download settles. Bridges that
  // spawn AFTER the download finishes pick it up automatically; bridges spawned
  // before will configure without ORT and semantic search will return its
  // existing "still building" status until the user restarts that session.
  //
  // The resolved path is passed to bridges via ORT_DYLIB_PATH env var.
  let onnxRuntimePromise: Promise<string | null> | null = null;
  if (aftConfig.semantic_search && isFastembedSemanticBackend) {
    const storageDir = configOverrides.storage_dir as string;
    onnxRuntimePromise = ensureOnnxRuntime(storageDir).catch((err) => {
      warn(
        `ONNX Runtime setup failed: ${err instanceof Error ? err.message : String(err)}. Semantic search will be unavailable.`,
      );
      return null;
    });
  }

  // ─────────────────────────── LSP auto-install ───────────────────────────
  //
  // Discover which LSPs the project actually needs, then surface every
  // already-cached binary directory to Rust as `lsp_paths_extra`. The Rust
  // resolver checks this list (after project-local node_modules and before
  // PATH), so any LSP we previously installed is found without users having
  // to put it on PATH.
  //
  // For LSPs that aren't yet cached, we kick off a background install (npm
  // for typescript-language-server / pyright / yaml-ls / bash-ls / dockerfile-ls
  // / @vue/language-server / @astrojs/language-server / svelte-language-server
  // / intelephense / @biomejs/biome; GitHub releases for clangd / lua-ls / zls
  // / tinymist / texlab). The 7-day grace window in `lsp.grace_days` defends
  // against newly-published malicious versions. Newly-installed binaries
  // appear in the cache for the user's NEXT plugin session — matching the
  // OpenCode "may need restart" UX and avoiding mid-session bridge restarts.
  //
  // The whole step is best-effort: if both probes fail, `cachedBinDirs` is
  // still populated from `isInstalled()` checks, so previously-installed
  // binaries continue to work.
  try {
    const lspAutoInstall = aftConfig.lsp?.auto_install ?? true;
    const lspGraceDays = aftConfig.lsp?.grace_days ?? 7;
    const lspVersions = aftConfig.lsp?.versions ?? {};
    const lspDisabled = new Set(aftConfig.lsp?.disabled ?? []);
    // When `lsp.auto_install: false`, leave the list empty so the Rust-side
    // `detect_missing_lsp_binaries` loop in configure.rs skips its built-in
    // server walk entirely. Without this gate, users who opted out of
    // auto-install still received `lsp_binary_missing` toasts/ignored-message
    // warnings on every configure. Explicit `lsp.servers` entries are
    // unaffected — those still warn (they're user-configured, not auto).
    configOverrides.lsp_auto_install_binaries = lspAutoInstall
      ? [...new Set([...NPM_LSP_TABLE, ...GITHUB_LSP_TABLE].map((spec) => spec.binary))]
      : [];

    const npmResult = runAutoInstall(input.directory, {
      autoInstall: lspAutoInstall,
      graceDays: lspGraceDays,
      versions: lspVersions,
      disabled: lspDisabled,
    });

    // GitHub-distributed servers gate on relevance separately because the
    // binaries are heavier (10-100 MB).
    const relevantGithub = discoverRelevantGithubServers(input.directory);
    const ghResult = runGithubAutoInstall(relevantGithub, {
      autoInstall: lspAutoInstall,
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
    // Both `runAutoInstall` and `runGithubAutoInstall` return synchronously
    // with the obvious skips (disabled, irrelevant, auto_install: false). The
    // backgrounded installs append additional reasons (grace blocked, registry
    // probe failed, install crashed) into `skipped` as their promises settle.
    //
    // We deliver ONE consolidated ignored message per session listing only
    // actionable reasons — the user can act on "grace blocked" (set a pin) or
    // "install failed" (check `/aft-status` and the plugin log), but not on
    // "not relevant to project" or "already installed" which are routine.
    //
    // Fire-and-forget; never block plugin startup.
    const installCompletion = Promise.all([npmResult.installsComplete, ghResult.installsComplete])
      .then(() => {
        if (installsWereStarted) {
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
          // Routine skips — don't notify.
          if (r === "auto_install: false") return false;
          if (r === "disabled by config") return false;
          if (r === "not relevant to project") return false;
          if (r === "already installed") return false;
          if (r === "another install in progress") return false;
          return true;
        });
        if (actionable.length > 0) {
          const lines = actionable.map((s) => `  • ${s.id}: ${s.reason}`).join("\n");
          const message =
            `AFT skipped or failed to install ${actionable.length} LSP server(s):\n${lines}\n\n` +
            "See `/aft-status` for details, or check the plugin log. " +
            'Pin a working version with `lsp.versions: { "<package>": "<version>" }` if grace is blocking, ' +
            "or set `lsp.auto_install: false` to suppress this entirely.";
          sendWarning({ client: input.client, directory: input.directory }, message).catch(
            (err) => {
              warn(`[lsp] failed to deliver install summary: ${err}`);
            },
          );
        }
        return updatedPaths;
      })
      .catch((err) => {
        warn(`[lsp] install-summary aggregation failed: ${err}`);
        return null;
      });
    if (installsWereStarted) lspInstallCompletion = installCompletion;
  } catch (err) {
    // Auto-install failures must never block plugin startup.
    warn(`[lsp] auto-install setup failed: ${err instanceof Error ? err.message : String(err)}`);
  }

  // Coordinate concurrent version mismatches so followers wait for the first
  // download/hot-swap for the target plugin version instead of failing with
  // "already attempted" while the compatible binary is still in flight.
  const versionUpgradePromises = new Map<string, Promise<string | null>>();

  const poolOptions: import("@cortexkit/aft-bridge").PoolOptions & {
    onBashLongRunning: (reminder: BashLongRunningPayload, bridge: BridgePendingState) => void;
    onBashPatternMatch: (frame: BashPatternMatchPayload, bridge: BridgePendingState) => void;
  } = {
    ...resolveBridgePoolTransportOptions(aftConfig),
    errorPrefix: "[aft-plugin]",
    minVersion: PLUGIN_VERSION,
    // Per-project configure overrides — fixes OpenCode Desktop /
    // `opencode serve` mode where one plugin instance serves many projects.
    // Without this, every bridge inherits the project config visible at
    // plugin init; with it, each project's `.cortexkit/aft.jsonc` wins for
    // that project's bridge. See PoolOptions.projectConfigLoader doc.
    projectConfigLoader: (projectRoot) => {
      try {
        if (!isProjectEnabled(projectRoot)) return {};
        deliverConfigMigrationWarnings(
          projectRoot,
          migrateAftConfigLocations(projectRoot, bridgeLogger).flatMap((result) => result.warnings),
        );
        return buildConfigTierConfigureParams(projectRoot);
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
      const existing = versionUpgradePromises.get(minVersion);
      if (existing) {
        log(
          `Version ${binaryVersion} < ${minVersion}; awaiting in-flight compatible binary upgrade`,
        );
        return existing;
      }

      const upgradePromise = (async () => {
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
    },
    onConfigureWarnings: ({ projectRoot, sessionId, client, warnings, configDroppedKeys }) => {
      const bridge = pool.getActiveBridgeForRoot(projectRoot);
      if (!bridge) return;
      const projectConfig = loadAftConfig(projectRoot);
      enqueueConfigureWarningsForSession({
        projectRoot,
        sessionId,
        client,
        bridge,
        warnings,
        configDroppedKeys,
        fallbackClient: input.client,
        storageDir: configOverrides.storage_dir as string,
        pluginVersion: PLUGIN_VERSION,
        serverUrl: input.serverUrl?.toString(),
        delivery: projectConfig.configure_warnings_delivery ?? "toast",
      });
    },
    onBashCompletion: (completion, bridge) => {
      // Use the callback bridge's project root: the pushed completion originated
      // from that bridge, so draining/acking against a session-dir cache fallback
      // can target the wrong project on cold/stale cache.
      const sessionDir = bridge.getCwd();
      void handlePushedBgCompletion(
        {
          ctx,
          directory: sessionDir,
          sessionID: completion.session_id,
          client: input.client,
        },
        completion,
      );
    },
    onBashLongRunning: (reminder, bridge) => {
      const sessionDir = bridge.getCwd();
      void handlePushedBgLongRunning(
        {
          ctx,
          directory: sessionDir,
          sessionID: reminder.session_id,
          client: input.client,
        },
        reminder,
      );
    },
    onBashPatternMatch: (frame, bridge) => {
      const sessionDir = bridge.getCwd();
      void handlePushedPatternMatch(
        {
          ctx,
          directory: sessionDir,
          sessionID: frame.session_id,
          client: input.client,
        },
        frame,
      );
    },
  };
  // SINGLE transport injection point (B-FINAL S4): standalone NDJSON bridge
  // (default) OR the subc daemon, selected by the USER-tier subc.connection_file.
  // Everything downstream is transport-agnostic behind AftTransportPool. Fails
  // loud if subc is selected but its connection file is absent (no silent
  // standalone downgrade).
  const pool = await createAftTransportPool({
    harness: "opencode",
    binaryPath,
    poolOptions,
    configOverrides,
    subcConnectionFile: aftConfig.subc?.connection_file,
    // Reaping-disabled pools have no root generation, so their nudges cannot carry
    // BgNudgeRef provenance. Keep the root/session callback wired as the delivery
    // path for those pools; bg-notifications coalesces it with provenance callbacks.
    onBgEventsNudge: (directory, sessionID) => {
      void handleSubcBgEventsNudge({
        ctx,
        directory,
        sessionID,
        client: input.client,
      }).catch((err) => {
        warn(`[aft-plugin] bg nudge rejected: ${err instanceof Error ? err.message : String(err)}`);
      });
    },
    onBgEventsNudgeRef: (ref) => {
      void handleSubcBgEventsNudge({
        ctx,
        directory: ref.canonicalRoot,
        sessionID: ref.session,
        nudgeRef: ref,
        client: input.client,
      }).catch((err) => {
        warn(`[aft-plugin] bg nudge rejected: ${err instanceof Error ? err.message : String(err)}`);
      });
    },
  });
  if (lspInstallCompletion) {
    lspInstallCompletion.then((updatedPaths) => {
      if (!updatedPaths) return;
      void pushLspPathsAfterAutoInstall(pool, input.directory, updatedPaths)
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
  pool.setConfigureOverride("harness", "opencode");
  const ctx: PluginContext = {
    pool,
    client: input.client,
    plugin: (input as { plugin?: PluginContext["plugin"] }).plugin,
    config: aftConfig,
    hashlineEffective,
    storageDir: configOverrides.storage_dir as string,
    isProjectEnabled,
  };

  type StatusSubscribableBridge = {
    subscribeStatus(listener: (snapshot: Record<string, unknown>) => void): () => void;
  };
  const statusSubscribedBridges = new WeakSet<object>();
  const statusUnsubscribes = new Set<() => void>();
  const subscribeBridgeStatus = (bridge: unknown): void => {
    if (!bridge || typeof bridge !== "object") return;
    if (statusSubscribedBridges.has(bridge)) return;
    const maybe = bridge as Partial<StatusSubscribableBridge>;
    if (typeof maybe.subscribeStatus !== "function") return;
    statusSubscribedBridges.add(bridge);
    const unsubscribe = maybe.subscribeStatus((snapshot) => {
      const session = snapshot.session as Record<string, unknown> | undefined;
      const sessionId = typeof session?.id === "string" ? session.id : undefined;
      pushStatusChange(sessionId);
    });
    statusUnsubscribes.add(unsubscribe);
  };
  const originalGetBridge = pool.getBridge.bind(pool);
  pool.getBridge = (projectRoot) => {
    if (!isProjectEnabled(projectRoot)) {
      throw new Error(`AFT disabled by config for ${projectRoot}`);
    }
    const bridge = originalGetBridge(projectRoot);
    subscribeBridgeStatus(bridge);
    return bridge;
  };
  const originalToolCall = pool.toolCall.bind(pool);
  pool.toolCall = async (projectRoot, runtime, name, rawArgs, options) => {
    const result = await originalToolCall(projectRoot, runtime, name, rawArgs, options);
    subscribeBridgeStatus(pool.getActiveBridgeForRoot(projectRoot));
    return result;
  };

  // Settle the ONNX runtime download promise (started above) and patch the
  // resolved path into the pool's configure overrides. Bridges spawned AFTER
  // this resolves will pass `_ort_dylib_dir` through configure and pick up
  // the runtime; bridges already running at resolution time keep going
  // without ORT (we don't restart them — that would discard warm
  // trigram/semantic/LSP state). Result: semantic search becomes available
  // for new sessions automatically once the download completes, without
  // forcing the user to restart OpenCode.
  if (onnxRuntimePromise) {
    onnxRuntimePromise.then(
      (ortDylibDir) => {
        if (ortDylibDir) {
          pool.setConfigureOverride("_ort_dylib_dir", ortDylibDir);
          log(`ONNX Runtime ready at ${ortDylibDir}; new bridges will load semantic backend.`);
        } else if (!isOrtAutoDownloadSupported()) {
          // Logged once; the manual-install warning is dispatched separately
          // through the warning channel below.
          log(`ONNX Runtime auto-download not supported on ${process.platform}/${process.arch}.`);
        }
      },
      (err) => {
        warn(`ONNX Runtime resolution rejected unexpectedly: ${err}`);
      },
    );
  }

  // Bridge spawn is lazy: the first tool call routed through `callBridge()`
  // (see `tools/_shared.ts`) creates the bridge on demand. Plugin init used
  // to fire-and-forget an eager configure here, but on OpenCode Desktop the
  // user typically has many projects open in the sidebar and only actively
  // uses one or two per session. Eager warmup spawned an `aft` process plus
  // watcher, LSP manager, and index loaders for every project at startup,
  // even ones the user never tool-touched — multiplying memory, CPU, and
  // file-watcher load by 10x or more for no benefit.
  //
  // ONNX Runtime resolution still happens in the background (kicked off
  // above). The `.then(...)` handler at line ~485 pushes `_ort_dylib_dir`
  // into the pool's configure overrides as soon as the download finishes,
  // so any bridge spawned later (including the first lazy spawn) picks it
  // up automatically. If a tool call lands before ONNX finishes, semantic
  // is unavailable on that specific bridge — same behavior as today on
  // first install, and a small price for skipping the eager wait.

  // Start RPC server for TUI plugin communication
  const rpcServer = new AftRpcServer(configOverrides.storage_dir as string, input.directory);

  // Install process-level SIGTERM/SIGINT handlers so that child `aft` processes
  // get an orderly shutdown when the Node host receives a termination signal.
  // Without this, OS propagates SIGTERM to children before OpenCode calls dispose,
  // and (together with bridge.ts signal handling) we want the shutdown path we
  // control, not implicit process-group death. Plugin dispose runs this same
  // cleanup set through runCleanups("dispose") so reloads do not leak children.
  let clearInspectTier2Idle = () => {};
  registerShutdownCleanup(async () => {
    autoUpdateAbort.abort();
    clearInspectTier2Idle();
    for (const unsubscribe of statusUnsubscribes) {
      try {
        unsubscribe();
      } catch {
        // Ignore unsubscribe errors during shutdown; sockets and other connections are already closing.
      }
    }
    statusUnsubscribes.clear();
    await Promise.allSettled([abortInFlightAutoInstalls(), abortInFlightGithubInstalls()]);
    try {
      rpcServer.stop();
    } catch {
      // best-effort
    }
    await pool.shutdown();
  });
  rpcServer.handle("status", async (params) => {
    const sessionID = (params.sessionID as string) || "rpc";
    // The TUI sidebar polls this every ~1.5s. We must NOT cold-spawn a bridge
    // just to answer a status query — the user may have launched OpenCode
    // from a directory that's expensive to configure (e.g. $HOME with 500k+
    // files), causing every poll to hang configure for 30s and restart
    // forever. If no bridge is already warm for this project, return a
    // synthetic "not_initialized" status so the sidebar shows something
    // sensible without triggering project indexing.
    //
    // Status is scoped to the POLLED SESSION's project, not this server's
    // launch cwd. With `opencode -s <sessid>` run from another project's
    // directory, OpenCode hands the plugin and the TUI the launch cwd — tool
    // calls already re-resolve the session's real directory via the SDK, and
    // the sidebar must match them, or it renders the launch-cwd project's
    // data for a session that lives elsewhere.
    //
    // Resolution order:
    //  1. SDK-verified session directory (fresh `session.get`, 15s memo) —
    //     NEVER the process-wide session-dir warm cache: in a multi-project
    //     host (Desktop / `opencode serve`) that cache is shared by all plugin
    //     instances and a fallback-seeded entry once made project A's server
    //     serve project B's bridge (RPC contamination).
    //  2. If the verified directory differs from ours and has NO warm bridge
    //     in this pool, return the placeholder — never another project's data
    //     for a foreign session; the client keeps scanning ports for the
    //     process that actually hosts that session's bridge.
    //  3. Own directory only when verification yields nothing (placeholder /
    //     empty session id, SDK miss) — the common single-project case.
    let bridge: ReturnType<typeof pool.getActiveBridgeForRoot> = null;
    let servedDirectory = input.directory;
    const realSessionID = (params.sessionID as string) || "";
    const verifiedDir = realSessionID
      ? await verifySessionDirectory(input.client, realSessionID)
      : null;
    if (verifiedDir) {
      if (!isProjectEnabled(verifiedDir)) {
        return {
          success: true,
          status: "disabled",
          message: `AFT disabled by config for ${verifiedDir}`,
        };
      }
      bridge = pool.getActiveBridgeForRoot(verifiedDir);
      if (bridge) {
        servedDirectory = verifiedDir;
      } else if (verifiedDir !== input.directory) {
        // No bridge for the session's project in THIS process. Hand the
        // SDK-verified directory back so the caller (TUI sidebar in a
        // different-cwd process, e.g. attached to a serve/Desktop host) can
        // re-scan that directory's port files and reach the process that
        // actually hosts the session's bridge.
        return {
          success: true,
          status: "not_initialized",
          verified_directory: verifiedDir,
          message:
            "AFT bridge is now spawned lazily, information here will be populated after first tool call.",
        };
      }
    }
    if (!bridge) {
      bridge = pool.getActiveBridgeForRoot(input.directory);
      servedDirectory = input.directory;
    }
    if (!bridge) {
      return {
        success: true,
        status: "not_initialized",
        ...(verifiedDir ? { verified_directory: verifiedDir } : {}),
        message:
          "AFT bridge is now spawned lazily, information here will be populated after first tool call.",
      };
    }
    // The cached snapshot is session-aware: Rust computes
    // `compression.session`, `session.checkpoints`, and `session.tracked_files`
    // for the *one* session_id passed at the time the cache was populated.
    // Serving that cached snapshot to a caller with a different sessionID
    // would mis-attribute another session's per-session slice — most visibly
    // showing `Session: 0 events` in the sidebar even when this session has
    // many compression events. Only serve the cache when its session matches.
    // `served_directory` is the cross-project provenance marker: it names the
    // project this server DELIBERATELY resolved for the caller (its own cwd,
    // or an SDK-verified resume directory). Clients reject mismatched-root
    // snapshots that lack a matching marker — that's what distinguishes a
    // legit resume serve from a stray multi-project-host response (old
    // servers, which could serve another project's bridge via the poisoned
    // session-dir cache, never set this field). Do NOT derive it from echoed
    // request params or the snapshot body.
    const cached = bridge.getCachedStatus();
    const cachedSessionId = (cached as Record<string, unknown> | null)?.session as
      | Record<string, unknown>
      | undefined;
    const cachedId = cachedSessionId?.id as string | undefined;
    if (cached !== null && cachedId === sessionID) {
      return { success: true, ...cached, served_directory: servedDirectory };
    }
    const response = await bridge.send("status", { session_id: sessionID });
    if (response.success !== false) {
      bridge.cacheStatusSnapshot(response);
    }
    return { ...response, served_directory: servedDirectory };
  });

  rpcServer.handle("consume-notifications", async (params) => {
    const rawLastReceivedId = Number(params.lastReceivedId ?? 0);
    // Scope drain to the TUI's active session so a notification tagged for a
    // different session (e.g. a dialog triggered by another client sharing this
    // process) is never delivered here. sessionId is optional for back-compat:
    // callers that omit it fall back to the previous unscoped behavior.
    const sessionId =
      typeof params.sessionId === "string" && params.sessionId.length > 0
        ? params.sessionId
        : undefined;
    const messages = drainNotifications(
      Number.isFinite(rawLastReceivedId) ? rawLastReceivedId : 0,
      sessionId,
    );
    return { messages };
  });
  // Feature announcement — TUI plugin calls this on startup to show a dialog.
  // Uses ANNOUNCEMENT_VERSION (not PLUGIN_VERSION) so patch releases don't re-fire.

  rpcServer.handle("get-announcement", async () => {
    if (!ANNOUNCEMENT_VERSION || ANNOUNCEMENT_FEATURES.length === 0) {
      return { show: false };
    }
    if (!storageDir) {
      // No storage path → we can't persist "seen" state, so suppress the
      // announcement to avoid spamming users whose storage isn't configured.
      return { show: false };
    }
    // shouldShowAnnouncement silently seeds the marker on first-install /
    // ephemeral-sandbox launches, so Docker/CI/disposable-VM users don't
    // see the changelog dialog every boot (per magic-context#99). Real
    // upgrades from a persisted older version still surface here.
    if (!shouldShowAnnouncement(storageDir, "opencode", ANNOUNCEMENT_VERSION)) {
      return { show: false };
    }
    return {
      show: true,
      version: ANNOUNCEMENT_VERSION,
      features: ANNOUNCEMENT_FEATURES,
      footer: ANNOUNCEMENT_FOOTER,
    };
  });

  rpcServer.handle("mark-announced", async () => {
    if (storageDir && ANNOUNCEMENT_VERSION) {
      markAnnouncementSeen(storageDir, "opencode", ANNOUNCEMENT_VERSION);
    }
    return { success: true };
  });

  rpcServer.handle("get-warnings", async () => {
    const warnings: string[] = [];
    if (
      aftConfig.semantic_search &&
      isFastembedSemanticBackend &&
      !configOverrides._ort_dylib_dir
    ) {
      if (!isOrtAutoDownloadSupported()) {
        warnings.push(`Semantic search requires ONNX Runtime.\nInstall: ${getManualInstallHint()}`);
      }
    }
    return { warnings };
  });

  rpcServer.start().catch((err) => warn(`RPC server failed to start: ${err}`));

  // --- Startup notifications (fire-and-forget, best-effort) ---
  const notifyOpts: NotificationOptions = {
    client: input.client,
    directory: input.directory,
  };

  // Feature announcements in TUI are handled by the TUI plugin via RPC (get-announcement + dialog).
  // In Desktop, sendFeatureAnnouncement sends an ignored message to the active session.
  // Both share the same last_announced_version file and the same ANNOUNCEMENT_VERSION
  // constant, so bugfix releases don't re-fire a stale dialog. No-op when empty.
  if (ANNOUNCEMENT_VERSION && ANNOUNCEMENT_FEATURES.length > 0) {
    setTimeout(() => {
      sendFeatureAnnouncement(
        notifyOpts,
        ANNOUNCEMENT_VERSION,
        ANNOUNCEMENT_FEATURES,
        ANNOUNCEMENT_FOOTER,
        storageDir,
      ).catch(() => {});
    }, 8000);
  }

  // Warn about ONNX Runtime if semantic search is enabled but ORT is unavailable.
  //
  // We branch on the promise we kicked off earlier rather than peeking at
  // configOverrides synchronously — the download is intentionally non-blocking
  // and the override is patched in only after it settles. If the promise
  // resolves to a path, no warning. If it resolves to null AND auto-download
  // is unsupported on this platform, surface the manual-install hint.
  if (onnxRuntimePromise) {
    onnxRuntimePromise.then(
      (ortDylibDir) => {
        if (!ortDylibDir && !isOrtAutoDownloadSupported()) {
          sendWarning(
            notifyOpts,
            `Semantic search requires ONNX Runtime.\nInstall: ${getManualInstallHint()}`,
          ).catch(() => {});
        }
      },
      () => {
        // Already logged in the .catch above; don't double-warn.
      },
    );
  } else {
    // No warnings needed — clean up any stale warnings from previous runs
    cleanupWarnings(notifyOpts).catch(() => {});
  }

  // Build the exact tool map for the configured profile. The builder contains
  // only registration work; startup, transport, and lifecycle hooks stay here.
  const allTools = buildAftToolDefinitions(ctx, aftConfig, (name, available) => {
    warn(`disabled_tools: "${name}" not found — available: ${available.join(", ")}`);
  });
  const disabled = aftConfig.disabled_tools ?? [];
  if (disabled.length > 0) {
    log(`Disabled ${disabled.length} tool(s): ${disabled.join(", ")}`);
  }

  // Wrap every tool's execute() with latency instrumentation: one log line per
  // call breaking total time into pre-bridge / bridge round-trip / post-bridge.
  // Applied after disabled-tool filtering so suppressed tools aren't wrapped.
  instrumentToolMap(allTools);

  const autoUpdateEventHook = createAutoUpdateCheckerHook(input, {
    enabled: true,
    autoUpdate: aftConfig.auto_update ?? true,
    signal: autoUpdateAbort.signal,
    // Multi-project plugin reloads coordinate via this on-disk timestamp
    // so the npm registry is hit at most once per check window across
    // every concurrent plugin instance on the machine.
    storageDir: ctx.storageDir,
  });

  // Workflow hints: short system-prompt block teaching token-efficient
  // AFT workflows. Computed from the final tool surface so we never
  // advertise tools the agent doesn't have. User-only — see config.ts
  // for the security rationale.
  // We pass the complement of registered tools (i.e. names that AREN'T in
  // allTools) so buildHintsFromConfig drops sections for tools the agent
  // can't actually call.
  const HINTS_TOOL_NAMES = [
    "aft_outline",
    "aft_zoom",
    "aft_search",
    "aft_callgraph",
    "aft_inspect",
    "grep",
    "aft_grep",
    "read",
    "aft_read",
    "bash",
    "aft_bash",
    "bash_status",
  ];
  const registeredTools = new Set(Object.keys(allTools));
  // The registration flag describes the hashline edit arm, not merely the
  // presence of the default edit tool. A config/schema mismatch must downgrade
  // Rust to the default surface instead of making both argument shapes fail.
  const hashlineEditRegistered = openCodeHashlineEditRegistered(aftConfig, registeredTools);
  ctx.hashlineEffective = hashlineEditRegistered;
  pool.setConfigureOverride("edit_slot_survives", hashlineEditRegistered);
  const hashlineDowngrade = openCodeHashlineDowngrade(aftConfig, registeredTools);
  log(
    `hashline activation decision requested=${aftConfig.edit_mode === "hashline"} ` +
      `edit_slot_survives=${hashlineEditRegistered} effective=${ctx.hashlineEffective}` +
      (hashlineDowngrade ? ` downgraded=${hashlineDowngrade.reason}` : ""),
  );
  const aftSearchRegistered = registeredTools.has("aft_search");
  // Tell Rust whether `aft_search` is registered for this surface so the
  // grep-rewrite footer can steer to it (vs the grep tool). The pool records
  // runtime overrides for lazy bridge spawns, so this reaches every bridge —
  // the same pattern used for `_ort_dylib_dir` and `lsp_paths_extra`.
  pool.setConfigureOverride("aft_search_registered", aftSearchRegistered);
  // Also expose the same surface decision to the TypeScript-side native bash
  // output finalizer, which catches leading grep/rg commands that Rust could
  // not rewrite (for example, greps with unsupported flags or pipes).
  (ctx as PluginContext & { aftSearchRegistered?: boolean }).aftSearchRegistered =
    aftSearchRegistered;
  // The bash tool description embeds a code-search prohibition that steers to
  // aft_search when registered (else the grep tool). Registration is only
  // known once the full tool map exists, so select the variant here — the
  // factory default assumes aft_search is absent. The compression and
  // background/PTY sentences are config-gated too: only advertised when the
  // feature is actually on for this project.
  for (const name of ["bash", "aft_bash"]) {
    const def = allTools[name];
    if (def) {
      const bashCfg = resolveBashConfig(aftConfig);
      def.description = bashToolDescription(
        aftSearchRegistered,
        bashCfg.compress,
        bashCfg.background,
        bashCfg.detach_on_user_message,
      );
    }
  }
  const hintsAbsentTools = new Set<string>();
  for (const name of HINTS_TOOL_NAMES) {
    if (!registeredTools.has(name)) hintsAbsentTools.add(name);
  }
  const hintsBlock = buildHintsFromConfig(aftConfig, hintsAbsentTools, hashlineEditRegistered);
  if (hintsBlock) {
    log(`Workflow hints injected (${hintsBlock.length} chars)`);
  }

  const inspectTier2Idle = createInspectTier2IdleScheduler({
    isEnabled: () => registeredTools.has("aft_inspect"),
    idleMinutes: () => aftConfig.inspect?.tier2_idle_minutes,
    warn,
    run: async (sessionID: string): Promise<void> => {
      const sessionDir =
        (await getSessionDirectory(input.client, sessionID, input.directory)) ?? input.directory;
      if (!isProjectEnabled(sessionDir)) return;
      const bridge = ctx.pool.getActiveBridgeForRoot(sessionDir) ?? ctx.pool.getBridge(sessionDir);
      const response = await bridge.send("inspect_tier2_run", { session_id: sessionID });
      if (response.success === false) {
        warn((response.message as string) || "inspect_tier2_run failed");
      }
    },
  });
  clearInspectTier2Idle = () => inspectTier2Idle.clearAll();

  return {
    tool: allTools,
    "experimental.chat.system.transform": async (
      _input: { sessionID?: string; model: unknown },
      output: { system: string[] },
    ) => {
      if (!hintsBlock) return;
      // Extend the existing entry instead of pushing a second system
      // message; strict Qwen-family templates reject non-leading system
      // messages. Mechanism documented on appendHintsToSystem.
      appendHintsToSystem(output.system, hintsBlock);
    },
    event: async (eventInput: { event: { type: string; properties?: unknown } }) => {
      await autoUpdateEventHook(eventInput);
      const eventType = eventInput.event.type;
      const sessionID = extractSessionID(eventInput.event.properties);
      // OpenCode's lifecycle vocabulary publishes session.deleted for explicit
      // remove() cleanup, not session.shutdown. Deletion-only cleanup is enough:
      // transport idle eviction and connection-exit quiesce handle abandoned
      // sessions, while in-flight bash aborts now use bash_abort_inflight.
      if (eventType === "session.deleted" && sessionID) {
        inspectTier2Idle.clear(sessionID);
        // Release this session's transport routes (subc: tool + bg_events;
        // standalone: no-op). Best-effort — never block the event hook.
        void pool.closeSession(input.directory, sessionID).catch(() => {});
        return;
      }
      if (eventType !== "session.idle") return;
      if (!sessionID) return;
      inspectTier2Idle.schedule(sessionID);
      // Use the session's stored directory rather than the plugin-init cwd:
      // OpenCode passes process.cwd() in `input.directory`, which can be wrong
      // for `-s` resumes from another folder.
      const sessionDir =
        (await getSessionDirectory(input.client, sessionID, input.directory)) ?? input.directory;
      if (!isProjectEnabled(sessionDir)) return;
      await handleIdleBgCompletions({
        ctx,
        directory: sessionDir,
        sessionID,
        client: input.client,
      });
      const configParseWarnings = drainPendingConfigParseWarnings(sessionDir);
      if (configParseWarnings.length > 0) {
        const bridge = pool.getActiveBridgeForRoot(sessionDir) ?? pool.getBridge(sessionDir);
        enqueueConfigureWarningsForSession({
          projectRoot: sessionDir,
          sessionId: sessionID,
          client: input.client,
          bridge,
          warnings: configParseWarnings,
          fallbackClient: input.client,
          storageDir: configOverrides.storage_dir as string,
          pluginVersion: PLUGIN_VERSION,
          serverUrl: input.serverUrl?.toString(),
          delivery: aftConfig.configure_warnings_delivery ?? "toast",
        });
      }
      await flushConfigureWarningsOnIdle(sessionID);
    },
    "chat.message": async (
      messageInput: {
        sessionID?: string;
        sessionId?: string;
        id?: string;
      },
      messageOutput?: { parts?: unknown },
    ) => {
      const sid = messageInput.sessionID ?? messageInput.sessionId ?? messageInput.id;
      // Eagerly warm the session-directory cache so the first tool call from
      // this turn routes to the right project (covers `opencode -s`-from-cwd).
      warmSessionDirectory(input.client, sid, input.directory);
      // Signal any in-flight sync bash_watch or wait:true foreground bash to
      // detach so the user's message is not blocked by a long-running wait.
      signalSyncWatchAbort(sid);
      if (!sid) return;
      const sessionDir =
        getSessionDirectoryCached(sid) ??
        (await getSessionDirectory(input.client, sid, input.directory)) ??
        input.directory;
      const projectConfig = loadAftConfig(sessionDir);
      const messageText = extractUserMessageText(messageOutput);
      const shouldDetach = shouldDetachBashWaitOnUserMessage(projectConfig, messageText);
      stripUserMessageDetachKeyword(messageOutput);
      if (shouldDetach) {
        void signalBashWaitDetachForProject(pool, sessionDir, sid);
      }
    },
    "tool.execute.before": async (
      toolInput: { tool: string; sessionID?: string },
      output: { args: unknown },
    ) => {
      // OpenCode invokes this hook with raw model arguments before applying the
      // registered schema. Normalize retired path spellings here so legacy
      // values are not stripped as unknown properties first.
      output.args = prepareOpenCodeArguments(toolInput.tool, output.args, {
        hashlineEffective: ctx.hashlineEffective,
      });
      if (toolInput.sessionID) inspectTier2Idle.clear(toolInput.sessionID);
    },
    "command.execute.before": async (
      commandInput: { command: string; sessionID: string },
      _output: unknown,
    ) => {
      if (commandInput.command !== STATUS_COMMAND) {
        return;
      }

      if (isTuiConnected(commandInput.sessionID)) {
        pushNotification(
          "action",
          { action: "show-status-dialog", sessionId: commandInput.sessionID },
          commandInput.sessionID,
        );
        throwSentinel(commandInput.command);
      }

      // Resolve the session's stored directory before picking a bridge —
      // otherwise `/aft-status` from a `-s` session would target home cwd.
      const sessionDir =
        (await getSessionDirectory(input.client, commandInput.sessionID, input.directory)) ??
        input.directory;
      if (!isProjectEnabled(sessionDir)) {
        await sendIgnoredMessage(
          input.client,
          commandInput.sessionID,
          `AFT disabled by config for ${sessionDir}`,
        );
        throwSentinel(commandInput.command);
      }
      // Prefer an existing active bridge to get warm index status
      const bridge = ctx.pool.getActiveBridgeForRoot(sessionDir) ?? ctx.pool.getBridge(sessionDir);
      // Cache is session-aware (Rust computes `session` / `compression.session`
      // for one specific session_id). Only serve it when its session matches
      // the caller's — otherwise we'd render another session's per-session
      // slice in this session's `/aft-status` dialog.
      const cached = bridge.getCachedStatus();
      const cachedSessionId = (cached as Record<string, unknown> | null)?.session as
        | Record<string, unknown>
        | undefined;
      const cachedId = cachedSessionId?.id as string | undefined;
      const cacheUsable = cached !== null && cachedId === commandInput.sessionID;
      const response = cacheUsable
        ? { success: true, ...cached }
        : await bridge.send("status", { session_id: commandInput.sessionID });
      if (!cacheUsable && response.success !== false) {
        bridge.cacheStatusSnapshot(response);
      }
      if (response.success === false) {
        throw new Error((response.message as string) || "status failed");
      }

      const status = coerceAftStatus(response);
      await sendIgnoredMessage(input.client, commandInput.sessionID, formatStatusMarkdown(status));
      throwSentinel(commandInput.command);
    },
    // Post-process tool output: append bash hints and drain in-turn background
    // completions. (UI title/diff metadata is
    // now returned directly from each tool's execute() — OpenCode's fromPlugin
    // preserves it — so the old metadata-store merge is gone; see #96.)
    "tool.execute.after": async (
      toolInput: { tool: string; sessionID: string; callID: string },
      output: { title: string; output: string; metadata: Record<string, unknown> } | undefined,
    ) => {
      if (!output) return;
      // Bash output hints — see shared/bash-hints.ts. The grep/rg code-search
      // redirect is emitted by the Rust bash rewriter (it owns the rewrite and
      // now reads `aft_search_registered` from config), so the plugin only adds
      // the conflicts hint here.
      if (toolInput.tool === "bash" && output.output) {
        output.output = maybeAppendConflictsHint(output.output);
      }
      // Use cached session directory so bg-completion drains target the
      // right project bridge after `opencode -s` from another cwd.
      const sessionDir = getSessionDirectoryCached(toolInput.sessionID) ?? input.directory;
      await appendInTurnBgCompletions(
        { ctx, directory: sessionDir, sessionID: toolInput.sessionID },
        output,
      );
    },
    config: async (config: { command?: Record<string, unknown> } | undefined) => {
      // Defensive guard: if OpenCode passes undefined or a non-object,
      // skip silently rather than crashing the plugin loader. The crash
      // surface here was responsible for `S.provider`/`z.config` errors
      // when this hook ran with an unexpected argument.
      if (!config || typeof config !== "object") return;
      // Register the only /aft-status slash entry. The TUI plugin registers a
      // palette-only command and receives dialog requests via RPC notifications.
      config.command = {
        ...(config.command ?? {}),
        [STATUS_COMMAND]: {
          template: STATUS_COMMAND,
          description: "Show AFT status, index health, cache usage, and runtime details",
        },
      };
    },
    dispose: async () => {
      await runCleanups("dispose");
    },
  };
}

export default plugin;
