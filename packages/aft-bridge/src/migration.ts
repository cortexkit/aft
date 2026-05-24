import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { findBinary } from "./resolver.js";

type SpawnSyncForMigration = typeof spawnSync;

let spawnSyncForMigration: SpawnSyncForMigration = spawnSync;

export function __setSpawnSyncForTests(impl: SpawnSyncForMigration | null): void {
  spawnSyncForMigration = impl ?? spawnSync;
}

export type MigrationHarness = "opencode" | "pi";

export interface MigrationOptions {
  harness: MigrationHarness;
  binaryPath?: string;
  logger?: {
    warn?: (msg: string) => void;
    info?: (msg: string) => void;
    log?: (msg: string) => void;
  };
  timeoutMs?: number;
}

export interface MigrationStatus {
  harness: MigrationHarness;
  target_root: string;
  migrated: boolean;
  marker_path?: string;
  migrated_at?: string;
  source_path?: string;
  aft_version?: string;
}

const TARGET_MARKER = ".migrated_from_legacy";
// Migration includes ONNX runtime (~200MB) plus user semantic/search indexes
// which can exceed 10GB in practice. Allow 30 minutes by default so large
// installs complete instead of being killed mid-copy.
const DEFAULT_TIMEOUT_MS = 30 * 60 * 1000;

function dataHome(): string {
  // We intentionally use XDG-style paths on macOS too, matching OpenCode's own
  // plugin storage convention. The legacy AFT storage on macOS lives at
  // ~/.local/share/opencode/storage/plugin/aft (not ~/Library/Application Support),
  // and the new CortexKit root must align so migration can find the source.
  if (process.env.XDG_DATA_HOME) return process.env.XDG_DATA_HOME;
  if (process.platform === "win32") {
    return process.env.LOCALAPPDATA || process.env.APPDATA || join(homeDir(), "AppData", "Local");
  }
  return join(homeDir(), ".local", "share");
}

function homeDir(): string {
  if (process.platform === "win32") return process.env.USERPROFILE || process.env.HOME || homedir();
  return process.env.HOME || homedir();
}

export function resolveLegacyStorageRoot(harness: MigrationHarness): string {
  if (harness === "pi") return join(homeDir(), ".pi", "agent", "aft");
  return join(dataHome(), "opencode", "storage", "plugin", "aft");
}

export function resolveCortexKitStorageRoot(): string {
  return join(dataHome(), "cortexkit", "aft");
}

function tail(value: string | undefined): string {
  if (!value) return "";
  return value.split("\n").slice(-20).join("\n").trim();
}

function spawnErrorLabel(error: Error): string {
  const code = "code" in error ? String((error as Error & { code?: unknown }).code ?? "") : "";
  return [code, error.message].filter(Boolean).join(": ");
}

function migrationLogPath(
  newRoot: string,
  harness: MigrationHarness,
  logger?: MigrationOptions["logger"],
): string {
  const desired = join(newRoot, "logs", "migration", `${harness}-${Date.now()}.jsonl`);
  try {
    mkdirSync(dirname(desired), { recursive: true });
    return desired;
  } catch (err) {
    const fallback = join(tmpdir(), `aft-migration-${harness}-${Date.now()}.jsonl`);
    logger?.warn?.(
      `Failed to create AFT migration log directory ${dirname(desired)}: ${err instanceof Error ? err.message : String(err)}. ` +
        `Using fallback log path ${fallback}.`,
    );
    return fallback;
  }
}

export async function ensureStorageMigrated(opts: MigrationOptions): Promise<void> {
  const legacyRoot = resolveLegacyStorageRoot(opts.harness);
  const newRoot = resolveCortexKitStorageRoot();
  const targetMarker = join(newRoot, opts.harness, TARGET_MARKER);
  const info = opts.logger?.info ?? opts.logger?.log;

  if (existsSync(targetMarker)) {
    info?.(`AFT storage already migrated for ${opts.harness}; using ${newRoot}`);
    return;
  }

  // Commit 4's migrate-storage treats a missing source as a no-op, but plugin
  // bootstrap should not require a binary just to discover a fresh install.
  // If only the legacy source marker exists, still invoke the migrator so it
  // can backfill the target marker idempotently on the next plugin boot.
  if (!existsSync(legacyRoot)) {
    info?.(
      `AFT storage migration skipped for ${opts.harness}: no legacy data at ${legacyRoot}; ` +
        `using ${newRoot} for fresh install`,
    );
    return;
  }

  const logPath = migrationLogPath(newRoot, opts.harness, opts.logger);
  const binaryPath = opts.binaryPath ?? (await findBinary());
  const startMs = Date.now();
  info?.(
    `AFT storage migration starting for ${opts.harness}: ${legacyRoot} -> ${newRoot} ` +
      `(binary=${binaryPath}, log=${logPath})`,
  );

  // User-visible notice. The migration spawn below is synchronous and can
  // take several minutes for large semantic/search indexes (>1GB). Without
  // a stderr message, the user sees OpenCode/Pi hang during plugin init
  // and may reasonably assume it's stuck. The host (OpenCode TUI, Pi TTY)
  // typically passes plugin stderr through to the user.
  try {
    process.stderr.write(
      `\n[AFT] Migrating ${opts.harness} storage to ${newRoot}.\n` +
        `[AFT] This may take several minutes for large indexes — please do not close ${opts.harness === "pi" ? "Pi" : "OpenCode"}.\n` +
        `[AFT] Source: ${legacyRoot}\n` +
        `[AFT] Log:    ${logPath}\n\n`,
    );
  } catch {
    // stderr may be unavailable in some sandboxed runtimes; the log file
    // still gets the same information.
  }

  const result = spawnSyncForMigration(
    binaryPath,
    [
      "migrate-storage",
      "--from",
      legacyRoot,
      "--to",
      newRoot,
      "--harness",
      opts.harness,
      "--log",
      logPath,
    ],
    {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      timeout: opts.timeoutMs ?? DEFAULT_TIMEOUT_MS,
    },
  );

  if (!result.error && result.status === 0) {
    const elapsedMs = Date.now() - startMs;
    info?.(
      `AFT storage migration completed for ${opts.harness} in ${elapsedMs}ms (log=${logPath})`,
    );
    try {
      process.stderr.write(`[AFT] Migration complete (${(elapsedMs / 1000).toFixed(1)}s).\n\n`);
    } catch {
      // see comment on start-message stderr write
    }
    return;
  }

  const detail = result.error
    ? `spawn error ${spawnErrorLabel(result.error)}`
    : result.status === null
      ? `terminated by signal ${result.signal ?? "unknown"}`
      : `exit ${result.status}`;
  const stderrTail = tail(result.stderr);
  const stdoutTail = tail(result.stdout);

  throw new Error(
    `AFT storage migration failed (${detail}). ` +
      `Harness: ${opts.harness}. Legacy: ${legacyRoot}. Target: ${newRoot}. ` +
      `See log: ${logPath}. ` +
      `Plugin load aborted to prevent legacy/new state divergence.` +
      (stderrTail ? ` Stderr tail: ${stderrTail}` : "") +
      (stdoutTail ? ` Stdout tail: ${stdoutTail}` : ""),
  );
}

/**
 * Query the migration status without performing any migration.
 * Used by `aft doctor` and similar diagnostics.
 */
export async function getMigrationStatus(opts: {
  harness: MigrationHarness;
  binaryPath?: string;
}): Promise<MigrationStatus> {
  const newRoot = resolveCortexKitStorageRoot();
  const binaryPath = opts.binaryPath ?? (await findBinary());
  const result = spawnSyncForMigration(
    binaryPath,
    ["migrate-storage", "--status", "--to", newRoot, "--harness", opts.harness],
    {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      timeout: DEFAULT_TIMEOUT_MS,
    },
  );

  if (result.error || result.status !== 0) {
    const detail = result.error
      ? `spawn error ${spawnErrorLabel(result.error)}`
      : result.status === null
        ? `terminated by signal ${result.signal ?? "unknown"}`
        : `exit ${result.status}`;
    const stderrTail = tail(result.stderr);
    const stdoutTail = tail(result.stdout);
    throw new Error(
      `AFT storage migration status failed (${detail}). ` +
        `Harness: ${opts.harness}. Target: ${newRoot}.` +
        (stderrTail ? ` Stderr tail: ${stderrTail}` : "") +
        (stdoutTail ? ` Stdout tail: ${stdoutTail}` : ""),
    );
  }

  try {
    return JSON.parse(result.stdout.trim()) as MigrationStatus;
  } catch (err) {
    throw new Error(
      `AFT storage migration status returned invalid JSON: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}
