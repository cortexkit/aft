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
 * None of those is automatable safely. But there's a fourth option AFT
 * actually owns end-to-end: clear `<storage_dir>/onnxruntime/` and let
 * the bridge re-download v1.24 on next start. That's what `--fix` does.
 *
 * Pair this with the resolver fix in `packages/aft-bridge/src/onnx-runtime.ts`
 * (which now skips system installs below v1.20) and the user gets a working
 * AFT-managed ONNX even when the system library is too old, with NO change
 * to system files.
 */

import { existsSync, rmSync } from "node:fs";
import { join } from "node:path";

import type { HarnessAdapter } from "../adapters/types.js";
import type { DiagnosticReport, HarnessDiagnostic } from "./diagnostics.js";
import { dirSize, formatBytes } from "./fs-util.js";
import { confirm, log, note } from "./prompts.js";

export interface OnnxFixCandidate {
  harness: HarnessDiagnostic;
  reason: string;
  storageOnnxDir: string;
  storageOnnxBytes: number;
}

/**
 * Identify harnesses where AFT's ONNX resolution is broken and could be
 * auto-fixed by clearing `<storage_dir>/onnxruntime/`. Each entry carries
 * a human-readable reason so the prompt explains exactly what's wrong.
 */
export function findOnnxFixCandidates(report: DiagnosticReport): OnnxFixCandidate[] {
  const candidates: OnnxFixCandidate[] = [];

  for (const harness of report.harnesses) {
    if (!harness.onnxRuntime.required) continue;
    if (!harness.storageDir.exists) continue;

    const storageOnnxDir = join(harness.storageDir.path, "onnxruntime");

    // Case 1: system install is too old AND no compatible cached install
    // exists. The resolver picks the system path, Rust rejects it. Clearing
    // storage/onnxruntime is a no-op here (nothing to clear), but the user
    // still benefits from being told the resolver fix in v0.19.5 takes care
    // of this on the next bridge start.
    const systemTooOld =
      harness.onnxRuntime.systemPath !== null && harness.onnxRuntime.systemCompatible === false;
    const cachedTooOld =
      harness.onnxRuntime.cachedPath !== null && harness.onnxRuntime.cachedCompatible === false;
    const hasCompatibleCached = harness.onnxRuntime.cachedCompatible === true;

    if (cachedTooOld) {
      candidates.push({
        harness,
        reason: `cached ONNX Runtime at ${harness.onnxRuntime.cachedPath} is v${harness.onnxRuntime.cachedVersion}, but AFT requires ${harness.onnxRuntime.requirement}. Clearing forces a fresh download on next start.`,
        storageOnnxDir,
        storageOnnxBytes: existsSync(storageOnnxDir) ? dirSize(storageOnnxDir) : 0,
      });
      continue;
    }

    if (systemTooOld && !hasCompatibleCached) {
      candidates.push({
        harness,
        reason: `system ONNX Runtime at ${harness.onnxRuntime.systemPath} is v${harness.onnxRuntime.systemVersion}, but AFT requires ${harness.onnxRuntime.requirement}, and no AFT-managed install is present. AFT v0.19.5+ skips incompatible system installs and auto-downloads v1.24 on next start; clearing any stale state here ensures a clean slate.`,
        storageOnnxDir,
        storageOnnxBytes: existsSync(storageOnnxDir) ? dirSize(storageOnnxDir) : 0,
      });
    }
  }

  return candidates;
}

export interface OnnxFixResult {
  cleared: number;
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
}

/**
 * Interactive ONNX fix flow. Returns the apply result (or null if no
 * fixable issues were found, or the user declined).
 *
 * Safety contract:
 *   - Only deletes paths inside `<storage_dir>/onnxruntime/` (AFT-owned).
 *   - NEVER touches `/usr/lib/...`, `/opt/homebrew/lib/...`, or any other
 *     system path.
 *   - Asks consent before any deletion (unless `options.yes` is set).
 *   - Reports byte counts so the user knows what's being reclaimed.
 */
export async function runOnnxFix(
  adapters: HarnessAdapter[],
  report: DiagnosticReport,
  options: OnnxFixOptions = {},
): Promise<OnnxFixResult | null> {
  const candidates = findOnnxFixCandidates(report);

  if (candidates.length === 0) {
    return null;
  }

  log.warn(
    `Found ${candidates.length} ONNX Runtime issue(s) that --fix can address by clearing AFT-managed cache:`,
  );
  for (const c of candidates) {
    log.info(`  • ${c.harness.displayName}: ${c.reason}`);
    if (c.storageOnnxBytes > 0) {
      log.info(`    will delete: ${c.storageOnnxDir} (${formatBytes(c.storageOnnxBytes)})`);
    } else {
      log.info(`    no AFT-managed ONNX cache to delete; nothing to reclaim`);
    }
  }

  note(
    "This NEVER touches system paths like /usr/lib. It only deletes AFT's own ONNX download cache. " +
      "On next bridge start, AFT will re-download ONNX Runtime v1.24 and use that instead of the " +
      "incompatible system library.",
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

  const result: OnnxFixResult = { cleared: 0, bytesReclaimed: 0, errors: [] };
  const rmFn = options.rmFn ?? rmSync;

  for (const c of candidates) {
    if (!existsSync(c.storageOnnxDir)) {
      // Nothing to delete — but the resolver fix in v0.19.5+ will still
      // produce a working install on next start. Report success so the
      // user sees consistent feedback.
      log.success(
        `${c.harness.displayName}: no cached state to clear; restart your harness to trigger a fresh ONNX download`,
      );
      continue;
    }
    try {
      rmFn(c.storageOnnxDir, { recursive: true, force: true });
      result.cleared += 1;
      result.bytesReclaimed += c.storageOnnxBytes;
      log.success(
        `${c.harness.displayName}: cleared ${c.storageOnnxDir} (reclaimed ${formatBytes(c.storageOnnxBytes)})`,
      );
    } catch (err) {
      const message = (err as Error).message ?? "unknown error";
      log.error(`${c.harness.displayName}: failed to clear ${c.storageOnnxDir}: ${message}`);
      result.errors.push({ path: c.storageOnnxDir, error: message });
    }
  }

  // Acknowledge the suppressed `adapters` argument — kept in the public
  // signature so future fixes (e.g. plugin re-registration, lsp cache
  // wipe) can use it without a breaking call-site change.
  void adapters;

  if (result.cleared > 0 || candidates.some((c) => c.storageOnnxBytes === 0)) {
    note(
      "Restart your AFT-using harness (OpenCode / Pi) to trigger a fresh ONNX Runtime download. " +
        "Watch the TUI sidebar — the Semantic Index status should move from 'failed' → 'building' → 'ready'.",
      "Next step",
    );
  }

  return result;
}
