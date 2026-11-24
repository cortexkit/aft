import {
  createGithubInstaller,
  type GithubAutoInstallOptions,
  type GithubAutoInstallResult,
  type GithubInstallConfig,
} from "@cortexkit/aft-bridge";
import { error, log, warn } from "./logger.js";
import {
  acquireInstallLock,
  readVersionCheck,
  shouldRecheckVersion,
  withInstallLock,
  writeVersionCheck,
} from "./lsp-cache.js";
import { hasRootMarker, relevantExtensionsInProject } from "./lsp-project-relevance.js";

export type {
  Arch,
  GithubAutoInstallResult,
  GithubInstallConfig,
  GithubServerSpec,
  Platform,
} from "@cortexkit/aft-bridge";
export { detectHostPlatform, findGithubServerById, GITHUB_LSP_TABLE } from "@cortexkit/aft-bridge";

const githubInstaller = createGithubInstaller({
  acquireInstallLock,
  log,
  warn,
  error,
  readVersionCheck,
  shouldRecheckVersion,
  withInstallLock,
  writeVersionCheck,
  hasRootMarker,
  relevantExtensionsInProject,
  // As in the Pi plugin: whether an install starts is decided later, after
  // grace, probe and lock checks, so nothing is reported as in flight.
  reportInstallingBinaries: false,
});

export const {
  ghBinaryPath,
  isGithubInstalled,
  __githubBinaryHashCountForTests,
  validateExtraction,
  precheckArchiveContents,
  abortInFlightGithubInstalls,
  discoverRelevantGithubServers,
  _assertAllowedDownloadUrlForTesting,
  _downloadFileForTesting,
  _extractArchiveSafelyForTesting,
  _precheckArchiveSizeForTesting,
  _runArchiveToolForTesting,
} = githubInstaller;

export const runGithubAutoInstall: (
  relevantServers: ReadonlySet<string>,
  config: GithubInstallConfig,
  fetchImpl?: typeof fetch,
  options?: GithubAutoInstallOptions,
) => GithubAutoInstallResult = githubInstaller.runGithubAutoInstall;
