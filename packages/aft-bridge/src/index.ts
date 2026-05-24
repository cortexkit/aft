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
export type {
  BashCompletedPayload,
  BashLongRunningPayload,
  BridgeOptions,
  BridgeRequestOptions,
  ConfigureWarning,
  ConfigureWarningsContext,
  StatusSnapshot,
} from "./bridge.js";
// --- transport ---
export { BinaryBridge, compareSemver, tagStderrLine } from "./bridge.js";
// --- binary resolution ---
export {
  downloadBinary,
  ensureBinary,
  getBinaryName,
  getCacheDir,
  getCachedBinaryPath,
} from "./downloader.js";
// --- compact UI formatting ---
export { compressionSavingsPercent, formatTokenCount } from "./format.js";
export type { Logger, LogMeta } from "./logger.js";
export type { MigrationHarness, MigrationOptions, MigrationStatus } from "./migration.js";
// --- storage migration ---
export {
  ensureStorageMigrated,
  getMigrationStatus,
  resolveCortexKitStorageRoot,
  resolveLegacyStorageRoot,
} from "./migration.js";
// --- ONNX runtime ---
export {
  __test__ as __onnxTest__,
  cleanupOnnxRuntime,
  ensureOnnxRuntime,
  getManualInstallHint,
  isOrtAutoDownloadSupported,
} from "./onnx-runtime.js";
// --- platform helpers ---
export { PLATFORM_ARCH_MAP, PLATFORM_ASSET_MAP } from "./platform.js";
export type { PoolOptions } from "./pool.js";
export { BridgePool, HomeProjectRootError, isHomeDirectoryRoot } from "./pool.js";
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
export { findBinary, findBinarySync, platformKey } from "./resolver.js";
// --- URL fetch (shared by aft_outline / aft_zoom URL targets) ---
export { _isPrivateIpv4, cleanupUrlCache, fetchUrlToTempFile } from "./url-fetch.js";
// --- aft_zoom plain-text formatter (shared by both plugin hosts) ---
export type { ZoomResponseLike } from "./zoom-format.js";
export { formatZoomText } from "./zoom-format.js";
