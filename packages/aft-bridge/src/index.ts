/**
 * @cortexkit/aft-bridge
 *
 * Shared transport, binary resolution, and ONNX runtime helpers for AFT
 * agent-host plugins. Public surface intentionally narrow — host policies
 * (config loading, permission UX, tool registration, notifications) stay in
 * each host plugin.
 */

// --- logger contract ---
export { setActiveLogger } from "./active-logger.js";
// --- bash output hints (shared by both plugin hosts) ---
export {
  formatForegroundResult,
  formatSeconds,
  isTerminalStatus,
  monotonicNowMs,
  sleep,
} from "./bash-format.js";
export {
  abortableSleep,
  commandInvokesCodeSearch,
  DEFAULT_PRIMARY_WATCH_TIMEOUT_MS,
  DEFAULT_WORKER_WAIT_MAX_MS,
  formatWaitDuration,
  formatWatchWaited,
  interruptedWatchTail,
  LONGEST_TIMER_DELAY_MS,
  MAX_WATCH_TIMEOUT_MS,
  MIN_WORKER_WAIT_MAX_MS,
  maxWatchTimeoutMs,
  maybeAppendConflictsHint,
  maybeAppendGrepSearchHint,
  outputTail,
  resolveWatchTimeoutMs,
  runningTaskStatusHint,
  taskKillDeadlineText,
  taskKillDeadlineWithinHandoffMargin,
  WATCH_SYNC_DEFAULTS_DESCRIPTION,
  WATCH_TIMEOUT_PARAM_DESCRIPTION,
  WATCH_UNAVAILABLE_GIVE_UP_MS,
  type WatchCallerRole,
  WORKER_KEEP_WAITING,
  WORKER_WAIT_LIMIT_PHRASE,
  watchClock,
  watchPollDelayMs,
  watchTimeoutSteer,
  watchUnavailableSteer,
  workerBackgroundTaskNote,
  workerWatchStillRunning,
} from "./bash-hints.js";
export {
  BASH_HOST_FALLBACK_BANNER,
  BASH_HOST_FALLBACK_MAX_OUTPUT_BYTES,
  BASH_HOST_FALLBACK_MAX_TIMEOUT_MS,
  BASH_HOST_FALLBACK_REFUSAL,
  BASH_RUNON_DESCRIPTION,
  BASH_RUNON_GUIDANCE,
  type BashHostFallbackOptions,
  type BashHostFallbackResult,
  bashHostFallbackAskPattern,
  runBashHostFallback,
} from "./bash-host-fallback.js";
// --- binary identity (no-exec cache trust) ---
export type { BinaryIdentity, BinaryIdentityCheck } from "./binary-identity.js";
export {
  __fileDigestWorkForTests,
  cachedFileSha256,
  cachedFileSha256Sync,
  checkBinaryIdentity,
  identitySidecarPath,
  isTrustedCachedBinary,
  readBinaryIdentity,
  writeStampedFileDigest,
} from "./binary-identity.js";
export type {
  BashCompletedPayload,
  BashLongRunningPayload,
  BridgeOptions,
  BridgeRequestOptions,
  ConfigureDroppedKey,
  ConfigureWarning,
  ConfigureWarningsContext,
  StatusSnapshot,
} from "./bridge.js";
// --- transport ---
export {
  BinaryBridge,
  BridgeTransportTimeoutError,
  BridgeTransportUnavailableError,
  BridgeTransportUnknownOutcomeError,
  compareSemver,
  isBridgeTransportTimeout,
  tagStderrLine,
} from "./bridge.js";
// --- cache paths and binary resolution ---
export {
  getAftBinaryCacheDir,
  getAftCacheRoot,
  getAftLspBinariesDir,
  getAftLspPackagesDir,
  getOpenCodeCacheRoot,
  getOpenCodeConfigRoot,
  type OpenCodePathEnvironment,
} from "./cache-paths.js";
// --- aft_callgraph flat formatter (shared by both plugin hosts) ---
export type { CallgraphFormatOptions, CallgraphTheme } from "./callgraph-format.js";
export { formatCallgraphSections, PLAIN_CALLGRAPH_THEME } from "./callgraph-format.js";
// --- child processes and PATH lookup: every spawn in the TypeScript packages
// goes through these so Windows never opens a console window for a child ---
export type {
  ChildProcess,
  ExecFileSyncOptions,
  ExecSyncOptions,
  SpawnOptions,
  SpawnSyncOptions,
  SpawnSyncReturns,
} from "./child-process.js";
export {
  execFile,
  execFileSync,
  execSync,
  spawn,
  spawnSync,
  withWindowsHidden,
} from "./child-process.js";
export {
  coerceAliasedStringParam,
  coerceBoolean,
  coerceJsonCollectionParam,
  coerceOptionalInt,
  coerceStringArray,
  coerceTargetParam,
  isBlankParam,
  isEmptyParam,
  isFindReplaceOnlyEdit,
  usableZoomTargets,
} from "./coerce.js";
export { LONG_RUNNING_COMMAND_TIMEOUT_MS, timeoutForCommand } from "./command-timeouts.js";
// --- config error state (a plugin that loads but fails every tool call) ---
export {
  AftConfigError,
  CONFIG_ERROR_RESTART_NOTE,
  type ConfigErrorCode,
  ConfigErrorTransportPool,
  configErrorStatusSnapshot,
  formatConfigErrorMessage,
  formatConfigErrorStatusLine,
  formatConfigParseErrorMessage,
  formatSubcConnectionMissingMessage,
  resolveSubcConnectionFilePath,
} from "./config-error.js";
// --- shared harness config keys ---
export {
  OPENCODE_ONLY_KEYS,
  PI_ONLY_KEYS,
  stripHarnessSpecificConfigKeys,
} from "./config-keys.js";
// --- config tiers ---
export type { ConfigTier } from "./config-tiers.js";
export { formatDroppedKeyWarnings, inlineUserConfigTier, readConfigTiers } from "./config-tiers.js";
export {
  aftLiveConfigKeys,
  aftLiveSecurityKeys,
  applyLiveConfigKeys,
  CONFIG_LIVE_KEEP_NOTE,
  CONFIG_WATCH_DEBOUNCE_MS,
  type LiveConfigApply,
  type LiveConfigKey,
  type LiveConfigLoad,
  type LiveConfigReload,
  type LiveConfigReloadOptions,
  type LiveSecurityKey,
  liveConfigReloadLogLine,
  type ResolvedBashForLiveReload,
  startLiveConfigReload,
  type WatchAftConfigFilesOptions,
  watchAftConfigFiles,
} from "./config-watch.js";
export {
  downloadBinary,
  ensureBinary,
  getBinaryName,
  getCacheDir,
  getCachedBinaryPath,
} from "./downloader.js";
export type { RotatingLogOptions } from "./durable-log.js";
// --- durable module/plugin logs ---
export {
  DEFAULT_LOG_BYTES,
  DEFAULT_LOG_GENERATIONS,
  RotatingLogSink,
} from "./durable-log.js";
export type { EditSummaryInput } from "./edit-summary.js";
export { formatEditSummary } from "./edit-summary.js";
// --- host-neutral agent tool errors ---
export {
  AftToolError,
  type AftToolErrorCause,
  adaptToolError,
  BASH_TRANSPORT_DISPOSITION,
  type BashHostFallbackCause,
  BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION,
  classifyBashHostFallbackError,
  isBashTransportDeadError,
  SUBC_ROUTE_CLOSED_MID_CALL_DISPOSITION,
  toolErrorFromResponse,
} from "./error-contract.js";
// --- feature-based configuration policy (shared with crates/aft feature_config.rs) ---
export * from "./feature-config.js";
// --- compact UI formatting ---
export { compressionSavingsPercent, formatTokenCount } from "./format.js";
// --- jsonc helpers ---
export { stripJsoncSymbols } from "./jsonc.js";
// --- per-Location process-global transport ownership ---
export type {
  AcquireBridgeDependencies,
  BridgeLifecycleCensus,
  BridgeLifecycleCensusOptions,
  BridgeLifecycleTopology,
} from "./location-lifecycle.js";
export {
  acquireBridge,
  getBridgeLifecycleTopology,
  releaseBridge,
  sampleBridgeLifecycleCensus,
} from "./location-lifecycle.js";
export type { Logger, LogMeta } from "./logger.js";
// --- which incoming messages interrupt a blocking bash wait ---
export {
  BASH_WAIT_DETACH_MAGIC_KEYWORD,
  containsStandaloneDetachKeyword,
  detachStripEdits,
  shouldInterruptWaitsForMessage,
  standaloneDetachKeywordRanges,
  stripDetachKeywordsAndTidyGap,
  stripStandaloneDetachKeywords,
} from "./message-detach.js";
export type {
  AftConfigFileMigrationOptions,
  AftConfigFileMigrationResult,
  MigrationHarness,
  MigrationOptions,
  MigrationStatus,
} from "./migration.js";
// --- storage migration ---
export {
  ensureStorageMigrated,
  getMigrationStatus,
  migrateAftConfigFile,
  resolveCortexKitStorageRoot,
  resolveLegacyStorageRoot,
} from "./migration.js";
export {
  clearMigrationNotices,
  deliverMigrationNoticeOnce,
  type MigrationNoticeOptions,
  migrationNoticeStorePath,
} from "./migration-notices.js";
// --- npm resolution (PATH-stripped GUI launch fallback) ---
export type { NpmInvocation, ResolvedNpm } from "./npm-resolver.js";
export {
  isNpmAvailable,
  NpmTerminationUnknownError,
  npmInvocation,
  npmSpawnEnv,
  probeNpmVersion,
  resolveNpm,
  terminateNpmProcessTree,
} from "./npm-resolver.js";
export { type OnnxRuntimeLoadProbe, probeOnnxRuntimeLoadable } from "./onnx-probe.js";
// --- ONNX runtime ---
export {
  __test__ as __onnxTest__,
  cleanupOnnxRuntime,
  ensureOnnxRuntime,
  getManualInstallHint,
  getOnnxRuntimeInstallFailure,
  isOrtAutoDownloadSupported,
} from "./onnx-runtime.js";
export {
  InvalidRequestError,
  isWellFormedUnicodeString,
  prepareCanonicalEditArguments,
  prepareCanonicalPathArguments,
} from "./path-aliases.js";
export { relativePathEscapesRoot, shortenHomePath } from "./path-display.js";
export { withPathPrepended } from "./path-env.js";
export {
  findExecutableOnPath,
  findExecutablesOnPath,
  type PathLookupOptions,
} from "./path-lookup.js";
export type { LegacyAftConfigSource, ResolvedAftConfigPaths } from "./paths.js";
export {
  decodeFileUrl,
  markAnnouncementSeen,
  repairRootScopedStorageFile,
  resolveCortexKitConfigPaths,
  resolveCortexKitProjectConfigPath,
  resolveCortexKitUserConfigPath,
  resolveHarnessStoragePath,
  resolveLegacyAftConfigSources,
  shouldShowAnnouncement,
} from "./paths.js";
// --- platform helpers ---
export { PLATFORM_ARCH_MAP, PLATFORM_ASSET_MAP } from "./platform.js";
export type { BridgeToolCallRuntime, PoolOptions } from "./pool.js";
export { BridgePool, HomeProjectRootError, isHomeDirectoryRoot } from "./pool.js";
// --- project-root identity (single canonicalizer; mirrors cortexkit-paths) ---
export { canonicalizeProjectRoot, projectRootKeyHash } from "./project-identity.js";
// --- wire contract ---
export type {
  AftErrorResponse,
  AftPushFrame,
  AftRequestEnvelope,
  AftResponse,
  AftSuccessResponse,
  BashCompletedFrame,
  BgCompletion,
  ConfigureWarningFrame,
  PermissionAskFrame,
  ProgressFrame,
  StatusCompression,
  StatusCompressionAggregate,
  StatusRemovalHealth,
  StatusResponse,
} from "./protocol.js";
export { findBinary, findBinarySync, isNativeExecutable, platformKey } from "./resolver.js";
export { RevivableTransportPool } from "./revivable-transport.js";
// --- semantic-index status vocabulary (shared by both plugin hosts) ---
export type { SemanticBackendDetail, SemanticIndexStatusKind } from "./semantic-status.js";
export { formatSemanticIndexStatus, semanticIndexStatusKind } from "./semantic-status.js";
export {
  isTestEnvironment,
  resolveAftLogPath,
  resolveAftStorageRoot,
  resolveDataHome,
  resolvePluginLogPath,
  resolveStoragePath,
  type StorageEnvironmentLookup,
  type StoragePathContext,
  type StoragePlatform,
} from "./storage-paths.js";
export {
  type BgNudgeRef,
  resolveBridgeForNudge,
  type SubcLocalRouteCloseReason,
  SubcRouteClosedMidCallError,
  SubcTransportPool,
  type SubcTransportPoolOptions,
  SubcTransportShuttingDownError,
} from "./subc-transport.js";
export { execTarExtractionSync, windowsTarExecutable } from "./tar-executable.js";
// --- shared agent-facing tool formatting ---
export type { ReadFooterOptions } from "./tool-format.js";
export { formatBridgeErrorMessage, formatReadFooter } from "./tool-format.js";
export type {
  AftProjectTransport,
  AftTransport,
  AftTransportOptions,
  AftTransportPool,
  ToolCallArguments,
  ToolCallOptions,
  ToolCallResult,
} from "./transport.js";
export {
  callPresetFor,
  observeFreshSessionStart,
  PRESET_FIELD,
  WORKER_SESSION_FIELD,
} from "./transport.js";
export {
  type AftTransportFactoryOptions,
  createAftTransportPool,
  subcConnectionFileError,
} from "./transport-factory.js";
export { readBinaryVersionOffThread } from "./version-probe.js";
// --- aft_zoom plain-text formatter (shared by both plugin hosts) ---
export type {
  RustZoomBatchEntry,
  ZoomMultiTargetEntry,
  ZoomMultiTargetResult,
  ZoomMultiTargetSymbolResult,
  ZoomResponseLike,
} from "./zoom-format.js";
export {
  formatZoomMultiTargetResult,
  formatZoomText,
  isRustZoomBatchEnvelope,
  unwrapRustZoomBatchEnvelope,
} from "./zoom-format.js";
