import {
  createAftTransportPool,
  getManualInstallHint,
  isOrtAutoDownloadSupported,
  markAnnouncementSeen,
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
  getActiveSessionId,
  handleIdleBgCompletions,
  handlePushedBgCompletion,
  handlePushedBgLongRunning,
  handlePushedPatternMatch,
  handleSubcBgEventsNudge,
  observeOpenCodeBgNotificationEvent,
} from "./bg-notifications.js";
import {
  applyToolSurfaceOverrides,
  createProjectAcceptance,
  createSharedPoolOptions,
  loadBootstrapConfig,
  prepareBridgeEnvironment,
  reportHashlineDowngrade,
  unknownDisabledToolsReporter,
} from "./bridge-bootstrap.js";
import {
  ConfigRejectedError,
  getConfigLoadErrors,
  loadAftConfig,
  resolveBashConfig,
  resolveBridgePoolTransportOptions,
  resolvedIndexes,
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
import { abortInFlightAutoInstalls } from "./lsp-auto-install.js";
import { abortInFlightGithubInstalls } from "./lsp-github-install.js";
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
import { registerShutdownCleanup } from "./shutdown-hooks.js";
import { signalSyncWatchAbort } from "./sync-watch-abort.js";
import { instrumentToolMap } from "./tool-perf.js";
import { buildAftToolDefinitions, openCodeHashlineEffective } from "./tool-registration.js";
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
const ANNOUNCEMENT_VERSION = "0.57.0";
const ANNOUNCEMENT_FEATURES: string[] = [
  "OpenCode 2 is supported: tools, permission prompts, the sidebar and cancellation all work on the GA host, and an end-to-end matrix guards them.",
  "`doctor` configures a machine that has only OpenCode 2 installed, and `--fix` no longer skips unrelated repairs when it cannot decide the generation.",
  "A code-health scan no longer starts while the same project is still building its call graph, which cost about 90 MB of peak memory on large repositories.",
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
 * - Host slots: read, write, edit, apply_patch, grep, glob, bash (+ companions)
 * - AST: ast_grep_search, ast_grep_replace
 * Every tool registers unless listed in `disabled_tools`.
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

  // Load the AFT config before any binary, storage or index work. A rejected
  // configuration (a retired key after its migration window, or an already
  // retired GitHub alias) publishes no AFT registrations at all. Load order:
  // ~/.config/cortexkit/aft.jsonc → <project>/.cortexkit/aft.jsonc
  const loadedConfig = loadBootstrapConfig(registrationRoot, (message) =>
    deliverConfigMigrationWarnings(registrationRoot, [message]),
  );
  if (!loadedConfig) {
    log(`AFT not started for ${registrationRoot}: its configuration was rejected`);
    return { tool: {} };
  }
  const aftConfig = loadedConfig;
  enqueueConfigParseWarnings(registrationRoot, getConfigLoadErrors());

  const notifyOpts: NotificationOptions = {
    client: input.client,
    directory: input.directory,
  };
  // Binary resolution, storage migration, the flat configure overrides, ONNX
  // Runtime, and LSP auto-install all come from the bootstrap shared with the
  // OpenCode 2 entry, so both hosts configure bridges identically.
  const bridgeEnvironment = await prepareBridgeEnvironment({
    configRoot: registrationRoot,
    lspDirectory: input.directory,
    config: aftConfig,
    pluginVersion: PLUGIN_VERSION,
    notify: (message) => {
      sendWarning(notifyOpts, message).catch((err) => {
        warn(`failed to deliver startup warning: ${err}`);
      });
    },
  });
  const { binaryPath, configOverrides, storageDir } = bridgeEnvironment;
  const onnxRuntimePromise = bridgeEnvironment.onnxRuntime;
  const autoUpdateAbort = new AbortController();
  // Reloads keep the last configuration that loaded successfully when the
  // current file is rejected.
  const lastGoodConfig = new Map<string, typeof aftConfig>([[registrationRoot, aftConfig]]);
  const loadAftConfigOrLastGood = (projectRoot: string): typeof aftConfig => {
    try {
      const loaded = loadAftConfig(projectRoot);
      lastGoodConfig.set(projectRoot, loaded);
      return loaded;
    } catch (err) {
      if (!(err instanceof ConfigRejectedError)) throw err;
      error(err.message);
      return lastGoodConfig.get(projectRoot) ?? aftConfig;
    }
  };
  // A project whose configuration is rejected gets no AFT work until it is
  // fixed; there is no longer a config switch that disables AFT wholesale.
  const isProjectEnabled = createProjectAcceptance(registrationRoot);

  // Configure params for the Rust binary come in two layers:
  //   1. `configOverrides` — GLOBAL per-process state shared by every bridge
  //      (see bridge-bootstrap.ts), plus raw config tiers for the init-time
  //      project so the first bridge configures without an extra loader call.
  //   2. `projectConfigLoader` — PER-BRIDGE raw config tiers, loaded from each
  //      project's own `.cortexkit/aft.jsonc` at bridge-spawn time.
  // The pool merges them with per-project values winning.
  //
  // Build tool schemas from the requested hashline setting. After disabled-tool
  // filtering, the code below freezes whether the final edit tool supports hashline.
  const hashlineEffective = openCodeHashlineEffective(aftConfig);
  const isFastembedSemanticBackend = (aftConfig.semantic?.backend ?? "fastembed") === "fastembed";

  const poolOptions: import("@cortexkit/aft-bridge").PoolOptions & {
    onBashLongRunning: (reminder: BashLongRunningPayload, bridge: BridgePendingState) => void;
    onBashPatternMatch: (frame: BashPatternMatchPayload, bridge: BridgePendingState) => void;
  } = {
    ...resolveBridgePoolTransportOptions(aftConfig),
    ...createSharedPoolOptions({
      pluginVersion: PLUGIN_VERSION,
      getPool: () => pool,
      isProjectEnabled,
      notifyForRoot: (projectRoot, message) =>
        deliverConfigMigrationWarnings(projectRoot, [message]),
    }),
    onConfigureWarnings: ({ projectRoot, sessionId, client, warnings, configDroppedKeys }) => {
      const bridge = pool.getActiveBridgeForRoot(projectRoot);
      if (!bridge) return;
      const projectConfig = loadAftConfigOrLastGood(projectRoot);
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
      const liveSession = getActiveSessionId();
      void handlePushedPatternMatch(
        {
          ctx,
          directory: sessionDir,
          sessionID: liveSession ?? frame.session_id,
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
  // Patches `_ort_dylib_dir` and late LSP install paths into the pool once
  // they settle; bridges spawned afterwards pick them up, running bridges keep
  // their warm state.
  bridgeEnvironment.attach(pool);
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

  // Bridge spawn is lazy: the first tool call routed through `callBridge()`
  // (see `tools/_shared.ts`) creates the bridge on demand. Plugin init used
  // to fire-and-forget an eager configure here, but on OpenCode Desktop the
  // user typically has many projects open in the sidebar and only actively
  // uses one or two per session. Eager warmup spawned an `aft` process plus
  // watcher, LSP manager, and index loaders for every project at startup,
  // even ones the user never tool-touched — multiplying memory, CPU, and
  // file-watcher load by 10x or more for no benefit.
  //
  // ONNX Runtime resolution still happens in the background (kicked off by
  // the bridge bootstrap). `bridgeEnvironment.attach` pushes `_ort_dylib_dir`
  // into the pool's configure overrides as soon as it resolves, so any bridge
  // spawned later (including the first lazy spawn) picks it up automatically.
  // If a tool call lands before a download finishes, semantic is unavailable
  // on that specific bridge, a small price for skipping the eager wait.

  // Start RPC server for TUI plugin communication
  const rpcServer = new AftRpcServer(configOverrides.storage_dir as string, input.directory);

  // Install process-level SIGTERM/SIGINT handlers so that child `aft` processes
  // get an orderly shutdown when the Node host receives a termination signal.
  // Without this, OS propagates SIGTERM to children before OpenCode calls dispose,
  // and (together with bridge.ts signal handling) we want the shutdown path we
  // control, not implicit process-group death. Instance disposal runs only this
  // registration; process-exit handlers retain the global drain.
  let clearInspectTier2Idle = () => {};
  const shutdownCleanup = registerShutdownCleanup(async (reason) => {
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
    await pool.shutdown(reason);
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
      resolvedIndexes(aftConfig).semantic &&
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

  // The missing-ONNX-Runtime warning is sent by the bridge bootstrap once
  // resolution settles. Without semantic search there is nothing to warn
  // about, so clear any stale warning from a previous run.
  if (!onnxRuntimePromise) {
    cleanupWarnings(notifyOpts).catch(() => {});
  }

  // Build the exact tool map for the configured profile. The builder contains
  // only registration work; startup, transport, and lifecycle hooks stay here.
  const allTools = buildAftToolDefinitions(
    ctx,
    aftConfig,
    unknownDisabledToolsReporter((message) =>
      deliverConfigMigrationWarnings(registrationRoot, [message]),
    ),
  );
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
    "read",
    "bash",
    "bash_status",
  ];
  const registeredTools = new Set(Object.keys(allTools));
  // The registration flag describes the hashline edit arm, not merely the
  // presence of the default edit tool. A config/schema mismatch must downgrade
  // Rust to the default surface instead of making both argument shapes fail.
  // `aft_search_registered` lets the Rust grep-rewrite footer steer to
  // aft_search (vs the grep tool). Both are shared with the OpenCode 2 entry.
  const { hashlineEditRegistered, aftSearchRegistered } = applyToolSurfaceOverrides(
    pool,
    aftConfig,
    registeredTools,
  );
  ctx.hashlineEffective = hashlineEditRegistered;
  // One configure-time warning per load; surviving slots keep ordinary behavior.
  const hashlineDowngrade = reportHashlineDowngrade(aftConfig, registeredTools, (message) =>
    deliverConfigMigrationWarnings(registrationRoot, [message]),
  );
  log(
    `hashline activation decision requested=${aftConfig.edit_mode === "hashline"} ` +
      `edit_slot_survives=${hashlineEditRegistered} effective=${ctx.hashlineEffective}` +
      (hashlineDowngrade ? ` downgraded=${hashlineDowngrade.code}` : ""),
  );
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
  for (const name of ["bash"]) {
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
      observeOpenCodeBgNotificationEvent(eventInput.event);
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
      const projectConfig = loadAftConfigOrLastGood(sessionDir);
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
      await shutdownCleanup.dispose("dispose");
    },
  };
}

export default plugin;
