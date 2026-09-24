/**
 * Once-per-identity delivery of configuration migration notices.
 *
 * A notice record is keyed by canonical config path, migration policy ID and
 * the `notice_projection_v1` digest of that file's migration-relevant inputs.
 * Unchanged inputs therefore warn once across restarts; any change to a
 * projected input changes the identity and warns again while a notice still
 * applies. Records are written only after delivery. Cross-process updates are
 * serialized with a lock directory; if the state directory is not writable the
 * notice is still delivered, together with a note that suppression cannot be
 * persisted. A crash between delivery and persistence can repeat a notice.
 */

import { mkdirSync, readFileSync, renameSync, rmdirSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";

import { MIGRATION_POLICY_ID } from "./feature-config.js";

type NoticeRecords = Record<string, { delivered_at: string }>;

/** `${XDG_STATE_HOME:-~/.local/state}/cortexkit/aft/migration-notices.json`. */
export function migrationNoticeStorePath(env: NodeJS.ProcessEnv = process.env): string {
  const stateHome =
    env.XDG_STATE_HOME && env.XDG_STATE_HOME.length > 0
      ? env.XDG_STATE_HOME
      : join(homedir(), ".local", "state");
  return join(stateHome, "cortexkit", "aft", "migration-notices.json");
}

function noticeKey(configPath: string, digest: string): string {
  return `${resolve(configPath)}|${MIGRATION_POLICY_ID}|${digest}`;
}

function readRecords(storePath: string): NoticeRecords {
  try {
    const parsed = JSON.parse(readFileSync(storePath, "utf8")) as unknown;
    return parsed && typeof parsed === "object" && !Array.isArray(parsed)
      ? (parsed as NoticeRecords)
      : {};
  } catch {
    return {};
  }
}

const LOCK_STALE_MS = 10_000;

function withStoreLock<T>(storePath: string, action: () => T): T {
  const lockPath = `${storePath}.lock`;
  mkdirSync(dirname(storePath), { recursive: true });
  const deadline = Date.now() + 2_000;
  for (;;) {
    try {
      mkdirSync(lockPath);
      break;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
      try {
        if (Date.now() - statSync(lockPath).mtimeMs > LOCK_STALE_MS) rmdirSync(lockPath);
      } catch {
        // Another process released or removed the lock; retry.
      }
      if (Date.now() > deadline) throw error;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 10);
    }
  }
  try {
    return action();
  } finally {
    try {
      rmdirSync(lockPath);
    } catch {
      // Best effort: a stale lock is reclaimed after LOCK_STALE_MS.
    }
  }
}

export interface MigrationNoticeOptions {
  configPath: string;
  digest: string;
  message: string;
  deliver: (message: string) => void;
  storePath?: string;
}

/**
 * Deliver a migration notice unless this exact identity was already
 * delivered. Returns true when the notice was delivered in this call.
 */
export function deliverMigrationNoticeOnce(options: MigrationNoticeOptions): boolean {
  const storePath = options.storePath ?? migrationNoticeStorePath();
  const key = noticeKey(options.configPath, options.digest);
  let delivered = false;
  try {
    return withStoreLock(storePath, () => {
      const records = readRecords(storePath);
      if (records[key] !== undefined) return false;
      options.deliver(options.message);
      delivered = true;
      records[key] = { delivered_at: new Date().toISOString() };
      const tmpPath = `${storePath}.tmp.${process.pid}`;
      writeFileSync(tmpPath, `${JSON.stringify(records, null, 2)}\n`, "utf8");
      renameSync(tmpPath, storePath);
      return true;
    });
  } catch (error) {
    const note = `AFT could not record this notice in ${storePath}, so it may repeat: ${
      error instanceof Error ? error.message : String(error)
    }`;
    options.deliver(delivered ? note : `${options.message} (${note})`);
    return true;
  }
}

/** Remove the records of one config file (used after a successful fix). */
export function clearMigrationNotices(configPath: string, storePath = migrationNoticeStorePath()) {
  const prefix = `${resolve(configPath)}|`;
  withStoreLock(storePath, () => {
    const records = readRecords(storePath);
    for (const key of Object.keys(records)) if (key.startsWith(prefix)) delete records[key];
    writeFileSync(storePath, `${JSON.stringify(records, null, 2)}\n`, "utf8");
  });
}
