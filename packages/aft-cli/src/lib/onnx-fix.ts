/**
 * ONNX Runtime auto-fix logic for `aft doctor --fix`.
 *
 * The most common ONNX failure mode in production is: a distro ships an
 * old `libonnxruntime.so` (Ubuntu 22.04 still has v1.9, etc.), AFT's
 * resolver picks it up, the Rust pre-validator rejects it as too old,
 * and semantic search shows "failed" forever in the TUI sidebar.
 *
 * The error message tells users to either:
 *   1. `rm /usr/lib/.../libonnxruntime.so` — needs sudo, breaks anything
 *      else linking that library, irreversible.
 *   2. Install ONNX 1.24 system-wide — manual, slow, distro-specific.
 *   3. Run doctor — diagnostics only.
 *
 * AFT owns a safe fourth option end-to-end: stage and verify a managed v1.24,
 * then transactionally replace only `<storage_dir>/onnxruntime/`. That's what
 * `--fix` does.
 *
 * Pair this with the resolver fix in `packages/aft-bridge/src/onnx-runtime.ts`
 * (which now skips system installs below v1.20) and the user gets a working
 * AFT-managed ONNX even when the system library is too old, with NO change
 * to system files.
 */

import { randomBytes } from "node:crypto";
import { existsSync, mkdirSync, renameSync, rmSync } from "node:fs";
import { isAbsolute, join, relative } from "node:path";
import { ensureOnnxRuntime } from "@cortexkit/aft-bridge";
import type { HarnessAdapter } from "../adapters/types.js";
import type { DiagnosticReport, HarnessDiagnostic } from "./diagnostics.js";
import { formatFsError } from "./fs-errors.js";
import { dirSize, formatBytes } from "./fs-util.js";
import { confirm, log, note } from "./prompts.js";

export interface OnnxFixCandidate {
  harness: HarnessDiagnostic;
  reason: string;
  storageOnnxDir: string;
  storageOnnxBytes: number;
}

/**
 * Identify harnesses where AFT can repair ONNX resolution by replacing its
 * managed cache and downloading a compatible runtime. Each entry carries a
 * human-readable reason so the prompt explains exactly what's wrong.
 */
export function findOnnxFixCandidates(report: DiagnosticReport): OnnxFixCandidate[] {
  const candidates: OnnxFixCandidate[] = [];

  for (const harness of report.harnesses) {
    if (!harness.onnxRuntime.required) continue;

    const storageOnnxDir = join(harness.storageDir.path, "onnxruntime");

    // A stale managed runtime needs replacement. An incompatible system
    // runtime is never deleted; the bridge resolver skips it while installing
    // the replacement in managed storage.
    const systemTooOld =
      harness.onnxRuntime.systemPath !== null && harness.onnxRuntime.systemCompatible === false;
    const cachedTooOld =
      harness.onnxRuntime.cachedPath !== null && harness.onnxRuntime.cachedCompatible === false;
    const hasCompatibleCached = harness.onnxRuntime.cachedCompatible === true;

    if (cachedTooOld) {
      candidates.push({
        harness,
        reason: `cached ONNX Runtime at ${harness.onnxRuntime.cachedPath} is v${harness.onnxRuntime.cachedVersion}, but AFT requires ${harness.onnxRuntime.requirement}. AFT will stage and verify a managed replacement before removing it.`,
        storageOnnxDir,
        storageOnnxBytes: existsSync(storageOnnxDir) ? dirSize(storageOnnxDir) : 0,
      });
      continue;
    }

    if (systemTooOld && !hasCompatibleCached) {
      candidates.push({
        harness,
        reason: `system ONNX Runtime at ${harness.onnxRuntime.systemPath} is v${harness.onnxRuntime.systemVersion}, but AFT requires ${harness.onnxRuntime.requirement}, and no AFT-managed install is present. AFT will leave the system copy untouched and download v1.24 into managed storage.`,
        storageOnnxDir,
        storageOnnxBytes: existsSync(storageOnnxDir) ? dirSize(storageOnnxDir) : 0,
      });
      continue;
    }

    if (!harness.onnxRuntime.systemPath && !harness.onnxRuntime.cachedPath) {
      const ignoredPath = harness.onnxRuntime.ignoredSystemPath;
      const ignoredReason = harness.onnxRuntime.ignoredSystemReason ?? "";
      const ignoredCopy = !ignoredPath
        ? ""
        : ignoredReason.startsWith("version unreadable")
          ? ` The Windows system copy at ${ignoredPath} was ignored because its version is unreadable.`
          : ` The system copy at ${ignoredPath} was ignored (${ignoredReason}).`;
      candidates.push({
        harness,
        reason: `no compatible ONNX Runtime is installed.${ignoredCopy} AFT will download v1.24 into managed storage.`,
        storageOnnxDir,
        storageOnnxBytes: existsSync(storageOnnxDir) ? dirSize(storageOnnxDir) : 0,
      });
    }
  }

  return candidates;
}

export interface OnnxFixResult {
  cleared: number;
  installed: number;
  bytesReclaimed: number;
  errors: { path: string; error: string }[];
}

export interface OnnxFixOptions {
  /** Skip the user prompt and act immediately (used by tests + scripted flows). */
  yes?: boolean;
  /** Inject a custom confirm impl for testing. */
  confirmFn?: (message: string, defaultYes?: boolean) => Promise<boolean>;
  /** Inject a custom rmSync impl for testing. */
  rmFn?: (path: string, options: { recursive: boolean; force: boolean }) => void;
  /** Inject the managed-runtime installer for testing. */
  ensureFn?: (storageDir: string) => Promise<string | null>;
}

interface ManagedReplacementResult {
  installedPath: string;
  clearedPrevious: boolean;
  cleanupError?: string;
}

async function installManagedReplacement(
  candidate: OnnxFixCandidate,
  ensureFn: NonNullable<OnnxFixOptions["ensureFn"]>,
  rmFn: NonNullable<OnnxFixOptions["rmFn"]>,
): Promise<ManagedReplacementResult> {
  const suffix = `${process.pid}.${Date.now().toString(36)}.${randomBytes(6).toString("hex")}`;
  const stagingStorageDir = `${candidate.harness.storageDir.path}.onnx-fix.tmp.${suffix}`;
  const stagedOnnxDir = join(stagingStorageDir, "onnxruntime");
  const backupDir = `${candidate.storageOnnxDir}.backup.${suffix}`;

  try {
    const stagedInstalledPath = await ensureFn(stagingStorageDir);
    if (!stagedInstalledPath || !existsSync(stagedInstalledPath)) {
      throw new Error("managed ONNX Runtime download was unavailable");
    }

    const installedRelativePath = relative(stagedOnnxDir, stagedInstalledPath);
    if (
      isAbsolute(installedRelativePath) ||
      installedRelativePath === ".." ||
      installedRelativePath.startsWith("../") ||
      installedRelativePath.startsWith("..\\")
    ) {
      throw new Error(`managed ONNX Runtime was installed outside staging: ${stagedInstalledPath}`);
    }

    mkdirSync(candidate.harness.storageDir.path, { recursive: true });
    const hadPrevious = existsSync(candidate.storageOnnxDir);
    if (hadPrevious) renameSync(candidate.storageOnnxDir, backupDir);

    try {
      renameSync(stagedOnnxDir, candidate.storageOnnxDir);
    } catch (installError) {
      if (hadPrevious) {
        try {
          renameSync(backupDir, candidate.storageOnnxDir);
        } catch (restoreError) {
          throw new Error(
            `managed ONNX Runtime replacement failed and the previous runtime could not be restored from ${backupDir}: ${restoreError}`,
            { cause: installError },
          );
        }
      }
      throw installError;
    }

    let cleanupError: string | undefined;
    if (hadPrevious) {
      try {
        rmFn(backupDir, { recursive: true, force: true });
      } catch (err) {
        cleanupError = err instanceof Error ? err.message : String(err);
      }
    }

    return {
      installedPath: join(candidate.storageOnnxDir, installedRelativePath),
      clearedPrevious: hadPrevious && cleanupError === undefined,
      cleanupError,
    };
  } finally {
    try {
      rmSync(stagingStorageDir, { recursive: true, force: true });
    } catch {
      // A leftover staging parent does not invalidate the published runtime.
    }
  }
}

export async function runOnnxFix(
  adapters: HarnessAdapter[],
  report: DiagnosticReport,
  options: OnnxFixOptions = {},
): Promise<OnnxFixResult | null> {
  const candidates = findOnnxFixCandidates(report);

  if (candidates.length === 0) return null;

  log.warn(
    `Found ${candidates.length} ONNX Runtime issue(s) that --fix can repair with an AFT-managed runtime:`,
  );
  for (const candidate of candidates) {
    log.info(`  • ${candidate.harness.displayName}: ${candidate.reason}`);
    if (candidate.storageOnnxBytes > 0) {
      log.info(
        `    will replace after a verified download: ${candidate.storageOnnxDir} (${formatBytes(candidate.storageOnnxBytes)})`,
      );
    } else {
      log.info("    no AFT-managed ONNX cache to delete");
    }
  }

  note(
    "This NEVER touches system paths like /usr/lib or C:\\Windows\\System32. It downloads and verifies a replacement before swapping AFT's own ONNX cache.",
    "Safe operation",
  );

  const confirmFn = options.confirmFn ?? confirm;
  const proceed = options.yes
    ? true
    : await confirmFn("Proceed with the fixes above?", /* defaultYes */ true);

  if (!proceed) {
    log.info("Skipped — no changes made.");
    return null;
  }

  const result: OnnxFixResult = { cleared: 0, installed: 0, bytesReclaimed: 0, errors: [] };
  const rmFn = options.rmFn ?? rmSync;
  const ensureFn = options.ensureFn ?? ensureOnnxRuntime;

  for (const candidate of candidates) {
    try {
      log.info(`${candidate.harness.displayName}: downloading managed ONNX Runtime…`);
      const replacement = await installManagedReplacement(candidate, ensureFn, rmFn);

      if (replacement.clearedPrevious) {
        result.cleared += 1;
        result.bytesReclaimed += candidate.storageOnnxBytes;
        log.success(
          `${candidate.harness.displayName}: replaced ${candidate.storageOnnxDir} (reclaimed ${formatBytes(candidate.storageOnnxBytes)})`,
        );
      }
      if (replacement.cleanupError) {
        log.warn(
          `${candidate.harness.displayName}: installed the replacement but could not remove the previous runtime backup: ${replacement.cleanupError}`,
        );
        result.errors.push({ path: candidate.storageOnnxDir, error: replacement.cleanupError });
      }

      result.installed += 1;
      log.success(
        `${candidate.harness.displayName}: ONNX Runtime installed at ${replacement.installedPath}`,
      );
    } catch (err) {
      const message = formatFsError(err, candidate.storageOnnxDir);
      log.error(`${candidate.harness.displayName}: ONNX Runtime download failed: ${message}`);
      result.errors.push({ path: candidate.storageOnnxDir, error: message });
    }
  }

  // Keep the adapter argument in the public signature for compatibility with
  // other doctor fixes that operate on harness adapters.
  void adapters;

  if (result.installed > 0) {
    note(
      "Restart your AFT-using harness (OpenCode / Pi) so semantic indexing uses the newly installed managed runtime.",
      "Next step",
    );
  }

  return result;
}
