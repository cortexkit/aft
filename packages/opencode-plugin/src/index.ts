import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";
import {
  BridgePool,
  cleanupUrlCache,
  ensureBinary,
  ensureOnnxRuntime,
  ensureStorageMigrated,
  findBinary,
  getManualInstallHint,
  isOrtAutoDownloadSupported,
  resolveCortexKitStorageRoot,
  setActiveLogger,
} from "@cortexkit/aft-bridge";
import type { Plugin } from "@opencode-ai/plugin";
import {
  appendInTurnBgCompletions,
  extractSessionID,
  handleIdleBgCompletions,
  handlePushedBgCompletion,
  handlePushedBgLongRunning,
} from "./bg-notifications.js";
import { loadAftConfig, resolveProjectOverridesForConfigure } from "./config.js";
import { createAutoUpdateCheckerHook } from "./hooks/auto-update-checker/index.js";
import { bridgeLogger, error, log, warn } from "./logger.js";
import { abortInFlightAutoInstalls, runAutoInstall } from "./lsp-auto-install.js";
import {
  abortInFlightGithubInstalls,
  discoverRelevantGithubServers,
  runGithubAutoInstall,
} from "./lsp-github-install.js";
import { GITHUB_LSP_TABLE } from "./lsp-github-table.js";
import { NPM_LSP_TABLE } from "./lsp-npm-table.js";
import { consumeToolMetadata } from "./metadata-store.js";
import { normalizeToolMap } from "./normalize-schemas.js";
import {
  cleanupWarnings,
  type NotificationOptions,
  sendFeatureAnnouncement,
  sendWarning,
} from "./notifications.js";
import { maybeAppendConflictsHint, maybeAppendGrepHint } from "./shared/bash-hints.js";
import { probeServerReachable, setLiveServerWakeAvailable } from "./shared/live-server-client.js";
import { AftRpcServer } from "./shared/rpc-server.js";
import {
  getSessionDirectory,
  getSessionDirectoryCached,
  warmSessionDirectory,
} from "./shared/session-directory.js";
import { coerceAftStatus, formatStatusMarkdown } from "./shared/status.js";
import { ensureTuiPluginEntry } from "./shared/tui-config.js";
import { registerShutdownCleanup } from "./shutdown-hooks.js";
import { astTools } from "./tools/ast.js";
import { conflictTools } from "./tools/conflicts.js";
import { aftPrefixedTools, hoistedTools } from "./tools/hoisted.js";
import { importTools } from "./tools/imports.js";
import { lspTools } from "./tools/lsp.js";
import { navigationTools } from "./tools/navigation.js";
import { readingTools } from "./tools/reading.js";
import { refactoringTools } from "./tools/refactoring.js";
import { safetyTools } from "./tools/safety.js";
import { searchTools } from "./tools/search.js";
import { semanticTools } from "./tools/semantic.js";
import { structureTools } from "./tools/structure.js";
import type { PluginContext } from "./types.js";
import { buildHintsFromConfig } from "./workflow-hints.js";

type BashLongRunningPayload = {
  session_id: string;
  task_id: string;
  command: string;
  elapsed_ms: number;
};

type BridgePendingState = {
  hasPendingRequests(): boolean;
};

// Register our logger with @cortexkit/aft-bridge before any bridge code runs.
// Module side-effect: import order matters because BridgePool / BinaryBridge
// internals call the active-logger helpers (log/warn/error) from constructors.
setActiveLogger(bridgeLogger);

const STATUS_COMMAND = "aft-status";
const SENTINEL_PREFIX = "__AFT_STATUS_";

function isTuiMode(): boolean {
  return process.env.OPENCODE_CLIENT === "cli";
}

// Slash commands are registered by the TUI plugin (tui/index.tsx) via api.command.register()
// which works in both TUI and Desktop modes. The server plugin only handles execution
// via command.execute.before hook (for Desktop rendering as ignored message).

function throwSentinel(command: string): never {
  throw new Error(`${SENTINEL_PREFIX}${command.toUpperCase().replace(/-/g, "_")}_HANDLED__`);
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
import {
  drainPendingEagerWarnings,
  handleConfigureWarningsForSession,
} from "./configure-warnings.js";

async function sendIgnoredMessage(client: unknown, sessionID: string, text: string): Promise<void> {
  const typedClient = client as {
    session?: {
      prompt?: (input: unknown) => unknown;
      promptAsync?: (input: unknown) => unknown;
    };
  };

  const body = {
    noReply: true,
    parts: [{ type: "text", text, ignored: true }],
  };
  const promptInput = { path: { id: sessionID }, body };

  if (typeof typedClient.session?.prompt === "function") {
    await Promise.resolve(typedClient.session.prompt(promptInput));
    return;
  }

  if (typeof typedClient.session?.promptAsync === "function") {
    await typedClient.session.promptAsync(promptInput);
    return;
  }

  throw new Error("[aft-plugin] client.session.prompt is unavailable");
}

/** Read the plugin's own version from package.json at build time. */
const PLUGIN_VERSION: string = (() => {
  try {
    const req = createRequire(import.meta.url);
    return (req("../package.json") as { version: string }).version;
  } catch {
    return "0.0.0";
  }
})();

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
const ANNOUNCEMENT_VERSION = "0.28.0";
const ANNOUNCEMENT_FEATURES: string[] = [
  "Bash hoisting is now default-on. Configure with top-level `bash: { rewrite, compress, background }` instead of `experimental.bash.*` — old config migrates automatically on first launch.",
  "Vue, Astro, and Svelte language servers now auto-install when the framework appears in your package.json (fixes #48).",
  "Native Windows ARM64 binary — ARM64 hosts no longer fall back to x64 under emulation.",
];

/**
 * Persistent footer rendered below the version-specific bullets in every
 * announcement. Stays in place across releases so users always see the Discord
 * invite without us needing to repeat it in `ANNOUNCEMENT_FEATURES` each time.
 *
 * Leave empty (`""`) to suppress.
 */
const ANNOUNCEMENT_FOOTER = "Join us on Discord: https://discord.gg/F2uWxjGnU";

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
 * - Structure: aft_transform
 * - Navigation: aft_navigate
 * - Refactoring: aft_refactor
 * - LSP: aft_lsp_diagnostics (inline diagnostics on edits are automatic)
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
  const binaryPath = await findBinary(PLUGIN_VERSION);

  await ensureStorageMigrated({ harness: "opencode", binaryPath, logger: bridgeLogger });

  // Load config: ~/.config/opencode/aft.jsonc → <project>/.opencode/aft.jsonc
  const aftConfig = loadAftConfig(input.directory);
  const autoUpdateAbort = new AbortController();

  // Build config overrides for the Rust binary (strip undefined values).
  //
  // **Two layers**:
  //   1. `configOverrides` (this block) — GLOBAL per-process state shared by
  //      every bridge: storage_dir, _ort_dylib_dir (patched later), harness,
  //      bash_permissions, lsp_paths_extra (LSP install cache).
  //   2. `projectConfigLoader` (wired below) — PER-BRIDGE, loaded from each
  //      project's own `.opencode/aft.jsonc` at bridge-spawn time. Contains
  //      everything that can legitimately differ per project: experimental.bash.*,
  //      format_on_edit, formatter, checker, restrict_to_project_root,
  //      search_index, semantic_search, semantic, lsp (per-project safe
  //      subset), max_callgraph_files.
  //
  // The pool merges them with per-project values winning. Without this split,
  // OpenCode Desktop / `opencode serve` (one plugin instance, many projects)
  // would burn the wrong project's config into every bridge — the project
  // visible at plugin init time would override everything.
  //
  // Seed the global layer with the init-time config so the FIRST bridge in
  // the init-time project still gets its config without an extra loader call.
  // Subsequent project switches re-resolve via the loader.
  const configOverrides: Record<string, unknown> = {
    ...resolveProjectOverridesForConfigure(aftConfig),
    bash_permissions: true,
  };

  const isFastembedSemanticBackend = (aftConfig.semantic?.backend ?? "fastembed") === "fastembed";

  // v0.27 stores runtime state under the shared CortexKit root. Migration from
  // the legacy OpenCode plugin root completed synchronously before any storage
  // consumer (ONNX, RPC server, bridge configure) can touch this path.
  configOverrides.storage_dir = resolveCortexKitStorageRoot();

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
    if (npmResult.installsStarted > 0 || ghResult.installsStarted > 0) {
      log(
        `[lsp] auto-install: ${npmResult.installsStarted} npm + ${ghResult.installsStarted} github install(s) running in background`,
      );
    }

    // ─── Surface install outcomes once installs settle (audit #6) ───
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
    Promise.all([npmResult.installsComplete, ghResult.installsComplete])
      .then(() => {
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
        if (actionable.length === 0) return;

        const lines = actionable.map((s) => `  • ${s.id}: ${s.reason}`).join("\n");
        const message =
          `AFT skipped or failed to install ${actionable.length} LSP server(s):\n${lines}\n\n` +
          "See `/aft-status` for details, or check the plugin log. " +
          'Pin a working version with `lsp.versions: { "<package>": "<version>" }` if grace is blocking, ' +
          "or set `lsp.auto_install: false` to suppress this entirely.";
        sendWarning({ client: input.client, directory: input.directory }, message).catch((err) => {
          warn(`[lsp] failed to deliver install summary: ${err}`);
        });
      })
      .catch((err) => {
        warn(`[lsp] install-summary aggregation failed: ${err}`);
      });
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
  } = {
    errorPrefix: "[aft-plugin]",
    minVersion: PLUGIN_VERSION,
    // Per-project configure overrides — fixes OpenCode Desktop /
    // `opencode serve` mode where one plugin instance serves many projects.
    // Without this, every bridge inherits the project config visible at
    // plugin init; with it, each project's `.opencode/aft.jsonc` wins for
    // that project's bridge. See PoolOptions.projectConfigLoader doc.
    projectConfigLoader: (projectRoot) => {
      try {
        const projectConfig = loadAftConfig(projectRoot);
        return resolveProjectOverridesForConfigure(projectConfig);
      } catch (err) {
        warn(
          `loadAftConfig(${projectRoot}) failed; falling back to plugin-init config: ${
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
    onConfigureWarnings: ({ projectRoot, sessionId, client, warnings }) => {
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
          fallbackClient: input.client,
          storageDir: configOverrides.storage_dir as string,
          pluginVersion: PLUGIN_VERSION,
        });
      }, 0);
    },
    onBashCompletion: (completion) => {
      // Prefer the cached session directory; fall back to plugin-init cwd
      // when we haven't seen this session yet (e.g. completion arriving
      // before any tool call has populated the cache).
      const sessionDir = getSessionDirectoryCached(completion.session_id) ?? input.directory;
      void handlePushedBgCompletion(
        {
          ctx,
          directory: sessionDir,
          sessionID: completion.session_id,
          // `client` is the in-process fallback used when the probe at
          // plugin init found the live HTTP listener unreachable. See
          // shared/live-server-client.ts and bg-notifications.ts for the
          // wake transport selection (anomalyco/opencode#28202).
          client: input.client,
          serverUrl: input.serverUrl?.toString(),
        },
        completion,
      );
    },
    onBashLongRunning: (reminder) => {
      const sessionDir = getSessionDirectoryCached(reminder.session_id) ?? input.directory;
      void handlePushedBgLongRunning(
        {
          ctx,
          directory: sessionDir,
          sessionID: reminder.session_id,
          // See onBashCompleted above for the live-server vs. in-process
          // wake transport selection.
          client: input.client,
          serverUrl: input.serverUrl?.toString(),
        },
        reminder,
      );
    },
  };
  const pool = new BridgePool(binaryPath, poolOptions, configOverrides);
  pool.setConfigureOverride("harness", "opencode");
  const ctx: PluginContext = {
    pool,
    client: input.client,
    plugin: (input as { plugin?: PluginContext["plugin"] }).plugin,
    config: aftConfig,
    storageDir: configOverrides.storage_dir as string,
  };

  // Wake transport probe: decide ONCE per plugin process whether
  // bg-notifications should POST wakes through a `createOpencodeClient`
  // aimed at `input.serverUrl` (the workaround for
  // anomalyco/opencode#28202 — no duplicate runs) or through the
  // in-process `input.client.session.promptAsync` (the upstream bug
  // path, but always available). Probe runs in the background so plugin
  // init never blocks on the HTTP timeout; the wake path reads the
  // resolved decision through `useLiveServerWake()` at the moment a
  // wake fires, so background probes that finish AFTER init still take
  // effect on the next reminder. Default until the probe resolves:
  // `false` (in-process fallback) — that's the safer direction because
  // `input.client.session.promptAsync` is always present, while the
  // live-server transport needs an actual listener to be reachable.
  void probeServerReachable(input.serverUrl?.toString())
    .then((reachable) => {
      setLiveServerWakeAvailable(reachable);
      log(
        reachable
          ? "Live OpenCode HTTP listener reachable; bg-notifications wake path = live-server (anomalyco/opencode#28202 workaround active)."
          : "Live OpenCode HTTP listener unreachable; bg-notifications wake path = in-process-fallback. Wakes will still arrive but the upstream duplicate-runner bug (anomalyco/opencode#28202) is not worked around. Launch with `opencode --port 0` in TUI mode to activate the workaround.",
      );
    })
    .catch(() => {
      // Probe failures stay on the safe default (in-process fallback).
      setLiveServerWakeAvailable(false);
    });
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
  // control, not implicit process-group death. The returned unregister is called
  // from dispose so plugin reloads don't leak stale cleanup callbacks.
  const unregisterShutdown = registerShutdownCleanup(async () => {
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
    // Try the session-stored directory first (fixes `opencode -s` from a
    // different cwd), then fall back to the plugin-init cwd.
    const cachedDir = getSessionDirectoryCached(sessionID);
    const candidateDirs = new Set<string>();
    if (typeof cachedDir === "string" && cachedDir.length > 0) {
      candidateDirs.add(cachedDir);
    }
    candidateDirs.add(input.directory);
    let bridge: ReturnType<typeof pool.getActiveBridgeForRoot> = null;
    for (const dir of candidateDirs) {
      bridge = pool.getActiveBridgeForRoot(dir);
      if (bridge) break;
    }
    if (!bridge) {
      return {
        success: true,
        status: "not_initialized",
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
    const cached = bridge.getCachedStatus();
    const cachedSessionId = (cached as Record<string, unknown> | null)?.session as
      | Record<string, unknown>
      | undefined;
    const cachedId = cachedSessionId?.id as string | undefined;
    if (cached !== null && cachedId === sessionID) {
      return { success: true, ...cached };
    }
    const response = await bridge.send("status", { session_id: sessionID });
    if (response.success !== false) {
      bridge.cacheStatusSnapshot(response);
    }
    return response;
  });
  // Feature announcement — TUI plugin calls this on startup to show a dialog.
  // Uses ANNOUNCEMENT_VERSION (not PLUGIN_VERSION) so patch releases don't re-fire.
  const storageDir = configOverrides.storage_dir as string;

  rpcServer.handle("get-announcement", async () => {
    if (!ANNOUNCEMENT_VERSION || ANNOUNCEMENT_FEATURES.length === 0) {
      return { show: false };
    }
    if (storageDir) {
      // v0.27 commit 11 deferral: the legacy `last_announced_version` file is read at
      // plugin init, BEFORE any bridge is spawned (lazy-spawn architecture per commit
      // 29508a5). Refactoring to `bridge.send("db_get_state")` would force eager bridge
      // spawn at every plugin init. Deferred to a future version that decides whether
      // to accept that trade-off. The Rust-side dual-write from commit 10 covers any
      // other writer; this file stays in sync via direct legacy-file writes.
      const versionFile = join(storageDir, "last_announced_version");
      try {
        if (existsSync(versionFile)) {
          const lastVersion = readFileSync(versionFile, "utf-8").trim();
          if (lastVersion === ANNOUNCEMENT_VERSION) return { show: false };
        }
      } catch {
        // proceed
      }
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
      try {
        mkdirSync(storageDir, { recursive: true });
        writeFileSync(join(storageDir, "last_announced_version"), ANNOUNCEMENT_VERSION);
      } catch {
        // best-effort
      }
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

  // Periodic URL cache cleanup (fire-and-forget, removes entries older than 24 hours)
  try {
    cleanupUrlCache(storageDir);
  } catch {
    // best-effort
  }

  try {
    ensureTuiPluginEntry();
  } catch {
    // Best-effort only
  }

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

  // Tool surface tiers:
  //   minimal:     aft_outline, aft_zoom, aft_safety
  //   recommended: minimal + hoisted + lsp_diagnostics + ast_grep_* + aft_import (default)
  //   all:         recommended + aft_navigate, aft_delete, aft_move, aft_transform, aft_refactor
  const surface = aftConfig.tool_surface ?? "recommended";

  // Tools only available in "all" tier
  const ALL_ONLY_TOOLS = new Set([
    "aft_navigate",
    "aft_delete",
    "aft_move",
    "aft_transform",
    "aft_refactor",
  ]);

  // Build full tool map
  const allTools = normalizeToolMap({
    // Hoisted tools: only in recommended+ (and when hoist_builtin_tools !== false)
    ...(surface !== "minimal" &&
      (aftConfig.hoist_builtin_tools !== false ? hoistedTools(ctx) : aftPrefixedTools(ctx))),
    ...readingTools(ctx),

    ...safetyTools(ctx),
    // aft_import: recommended+
    ...(surface !== "minimal" && importTools(ctx)),
    ...structureTools(ctx),
    ...navigationTools(ctx),
    // AST tools: recommended+
    ...(surface !== "minimal" && astTools(ctx)),
    ...(surface !== "minimal" && aftConfig.semantic_search === true && semanticTools(ctx)),
    // Indexed search tools: recommended+ and opt-in
    ...(surface !== "minimal" && aftConfig.search_index === true && searchTools(ctx)),
    ...refactoringTools(ctx),
    // LSP diagnostics: recommended+
    ...(surface !== "minimal" && lspTools(ctx)),
    // Git conflicts: recommended+
    ...(surface !== "minimal" && conflictTools(ctx)),
  });

  // Remove all-only tools when surface is minimal or recommended
  if (surface !== "all") {
    for (const name of ALL_ONLY_TOOLS) {
      if (name in allTools) {
        delete allTools[name];
      }
    }
  }

  // Filter disabled tools (user + project config union)
  const disabled = new Set(aftConfig.disabled_tools ?? []);
  if (disabled.size > 0) {
    for (const name of disabled) {
      if (name in allTools) {
        delete allTools[name];
      } else {
        warn(
          `disabled_tools: "${name}" not found — available: ${Object.keys(allTools).join(", ")}`,
        );
      }
    }
    log(`Disabled ${disabled.size} tool(s): ${[...disabled].join(", ")}`);
  }

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
    "aft_navigate",
    "grep",
    "aft_grep",
    "bash",
    "aft_bash",
    "bash_status",
  ];
  const registeredTools = new Set(Object.keys(allTools));
  const hintsAbsentTools = new Set<string>();
  for (const name of HINTS_TOOL_NAMES) {
    if (!registeredTools.has(name)) hintsAbsentTools.add(name);
  }
  const hintsBlock = buildHintsFromConfig(aftConfig, hintsAbsentTools);
  if (hintsBlock) {
    log(`Workflow hints injected (${hintsBlock.length} chars)`);
  }

  return {
    tool: allTools,
    "experimental.chat.system.transform": async (
      _input: { sessionID?: string; model: unknown },
      output: { system: string[] },
    ) => {
      if (hintsBlock) {
        output.system.push(hintsBlock);
      }
    },
    event: async (eventInput: { event: { type: string; properties?: unknown } }) => {
      await autoUpdateEventHook(eventInput);
      if (eventInput.event.type !== "session.idle") return;
      const sessionID = extractSessionID(eventInput.event.properties);
      if (!sessionID) return;
      // Use the session's stored directory rather than the plugin-init cwd:
      // OpenCode passes process.cwd() in `input.directory`, which can be wrong
      // for `-s` resumes from another folder.
      const sessionDir =
        (await getSessionDirectory(input.client, sessionID, input.directory)) ?? input.directory;
      await handleIdleBgCompletions({
        ctx,
        directory: sessionDir,
        sessionID,
        client: input.client,
        serverUrl: input.serverUrl?.toString(),
      });
    },
    "chat.message": async (messageInput: {
      sessionID?: string;
      sessionId?: string;
      id?: string;
    }) => {
      const sid = messageInput.sessionID ?? messageInput.sessionId ?? messageInput.id;
      // Eagerly warm the session-directory cache so the first tool call from
      // this turn routes to the right project (covers `opencode -s`-from-cwd).
      warmSessionDirectory(input.client, sid, input.directory);
    },
    "command.execute.before": async (
      commandInput: { command: string; sessionID: string },
      _output: unknown,
    ) => {
      if (isTuiMode() || commandInput.command !== STATUS_COMMAND) {
        return;
      }

      // Resolve the session's stored directory before picking a bridge —
      // otherwise `/aft-status` from a `-s` session would target home cwd.
      const sessionDir =
        (await getSessionDirectory(input.client, commandInput.sessionID, input.directory)) ??
        input.directory;
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
    // Restore metadata that fromPlugin() overwrites (opencode bug workaround)
    "tool.execute.after": async (
      toolInput: { tool: string; sessionID: string; callID: string },
      output: { title: string; output: string; metadata: Record<string, unknown> } | undefined,
    ) => {
      if (!output) return;
      const stored = consumeToolMetadata(toolInput.sessionID, toolInput.callID);
      if (stored) {
        if (stored.title) output.title = stored.title;
        if (stored.metadata) output.metadata = { ...output.metadata, ...stored.metadata };
      }
      // Bash output hints — see shared/bash-hints.ts for the gating logic.
      if (toolInput.tool === "bash" && output.output) {
        output.output = maybeAppendConflictsHint(output.output);
        output.output = maybeAppendGrepHint(output.output);
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
      // Register /aft-status for Desktop command palette.
      // In TUI mode, the TUI plugin also registers it via api.command.register()
      // which takes priority for dialog rendering.
      config.command = {
        ...(config.command ?? {}),
        [STATUS_COMMAND]: {
          template: STATUS_COMMAND,
          description: "Show AFT status, index health, cache usage, and runtime details",
        },
      };
    },
    dispose: async () => {
      autoUpdateAbort.abort();
      unregisterShutdown();
      await Promise.allSettled([abortInFlightAutoInstalls(), abortInFlightGithubInstalls()]);
      rpcServer.stop();
      await pool.shutdown();
    },
  };
}

export default plugin;
