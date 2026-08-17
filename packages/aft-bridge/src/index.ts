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
  sleep,
} from "./bash-format.js";
export {
  commandInvokesCodeSearch,
  maybeAppendConflictsHint,
  maybeAppendGrepSearchHint,
} from "./bash-hints.js";
export {
  BASH_HOST_FALLBACK_BANNER,
  BASH_HOST_FALLBACK_MAX_OUTPUT_BYTES,
  BASH_HOST_FALLBACK_MAX_TIMEOUT_MS,
  BASH_HOST_FALLBACK_REFUSAL,
  type BashHostFallbackOptions,
  type BashHostFallbackResult,
  bashHostFallbackAskPattern,
  runBashHostFallback,
} from "./bash-host-fallback.js";
export { resolveBashKillTimeout } from "./bash-timeout.js";
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
} from "./cache-paths.js";
// --- aft_callgraph flat formatter (shared by both plugin hosts) ---
export type { CallgraphFormatOptions, CallgraphTheme } from "./callgraph-format.js";
export { formatCallgraphSections, PLAIN_CALLGRAPH_THEME } from "./callgraph-format.js";
export {
  coerceAliasedStringParam,
  coerceBoolean,
  coerceOptionalInt,
  coerceStringArray,
  coerceTargetParam,
  isEmptyParam,
} from "./coerce.js";
export { LONG_RUNNING_COMMAND_TIMEOUT_MS, timeoutForCommand } from "./command-timeouts.js";
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
  resolveAftLogPath,
  resolveAftStorageRoot,
} from "./durable-log.js";
export type { EditSummaryInput } from "./edit-summary.js";
export { formatEditSummary } from "./edit-summary.js";
// --- host-neutral agent tool errors ---
export {
  AftToolError,
  type AftToolErrorCause,
  adaptToolError,
  BASH_TRANSPORT_DISPOSITION,
  BRIDGE_TRANSPORT_UNKNOWN_OUTCOME_DISPOSITION,
  isBashTransportDeadError,
  toolErrorFromResponse,
} from "./error-contract.js";
// --- compact UI formatting ---
export { compressionSavingsPercent, formatTokenCount } from "./format.js";
// --- jsonc helpers ---
export { stripJsoncSymbols } from "./jsonc.js";
export type { Logger, LogMeta } from "./logger.js";
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
// --- npm resolution (PATH-stripped GUI launch fallback) ---
export type { ResolvedNpm } from "./npm-resolver.js";
export {
  isNpmAvailable,
  npmSpawnEnv,
  probeNpmVersion,
  resolveNpm,
} from "./npm-resolver.js";
// --- ONNX runtime ---
export {
  __test__ as __onnxTest__,
  cleanupOnnxRuntime,
  ensureOnnxRuntime,
  getManualInstallHint,
  isOrtAutoDownloadSupported,
} from "./onnx-runtime.js";
export {
  InvalidRequestError,
  isWellFormedUnicodeString,
  prepareCanonicalEditArguments,
  prepareCanonicalPathArguments,
} from "./path-aliases.js";
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
  StatusResponse,
} from "./protocol.js";
export { findBinary, findBinarySync, isNativeExecutable, platformKey } from "./resolver.js";
export { RevivableTransportPool } from "./revivable-transport.js";
export {
  type BgNudgeRef,
  resolveBridgeForNudge,
  SubcTransportPool,
  type SubcTransportPoolOptions,
  SubcTransportShuttingDownError,
} from "./subc-transport.js";
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
  type AftTransportFactoryOptions,
  createAftTransportPool,
} from "./transport-factory.js";
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
