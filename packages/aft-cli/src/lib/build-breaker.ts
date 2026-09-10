import { existsSync, readdirSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";
import type { DatabaseSync } from "node:sqlite";

import { CLI } from "./cli.js";

/// `node:sqlite` is loaded on use, never at module load: the CLI is one bundle,
/// so a static import would make every command (including `setup`) fail to
/// link on Node < 22.5 with ERR_UNKNOWN_BUILTIN_MODULE before it could say why.
function loadDatabaseSync(): typeof DatabaseSync {
  const require = createRequire(import.meta.url);
  try {
    return (require("node:sqlite") as { DatabaseSync: typeof DatabaseSync }).DatabaseSync;
  } catch (error) {
    const code = (error as { code?: string })?.code;
    if (code === "ERR_UNKNOWN_BUILTIN_MODULE" || code === "MODULE_NOT_FOUND") {
      throw new Error(
        `${CLI} needs Node.js 22.5 or newer for this command (node:sqlite is unavailable on ${process.version})`,
      );
    }
    throw error;
  }
}

export const DOCTOR_BUILD_BREAKER_RESET_COMMAND = `${CLI} doctor reset-build-breaker`;

export interface BuildBreakerSuspension {
  root: string;
  domain: string;
  reason: string;
  deathCount: number;
  ageS: number;
  fingerprint: string;
}

interface BuildBreakerRow {
  root_id: string;
  domain: string;
  corpus_fingerprint: string;
  zero_credit_deaths: number;
  credited_deaths: number;
  suspended_reason: string;
  suspended_since_ms: number;
}

function buildBreakerDatabases(storageRoot: string): string[] {
  const callgraphRoot = join(storageRoot, "callgraph");
  if (!existsSync(callgraphRoot)) return [];
  try {
    return readdirSync(callgraphRoot, { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .map((entry) => join(callgraphRoot, entry.name, "build-breaker.sqlite"))
      .filter((path) => existsSync(path));
  } catch {
    return [];
  }
}

/**
 * Read active trips directly from the durable breaker database. The CLI never
 * guesses a suspension from logs or cache freshness, keeping doctor aligned
 * with the runtime surfaces that consult the same rows.
 */
export function readBuildBreakerSuspensions(
  storageRoot: string,
  nowMs: number = Date.now(),
): BuildBreakerSuspension[] {
  const suspensions: BuildBreakerSuspension[] = [];
  for (const databasePath of buildBreakerDatabases(storageRoot)) {
    let database: DatabaseSync | undefined;
    try {
      database = new (loadDatabaseSync())(databasePath, { readOnly: true });
      const rows = database
        .prepare(
          `SELECT root_id, domain, corpus_fingerprint, zero_credit_deaths, credited_deaths,
                  suspended_reason, suspended_since_ms
             FROM breaker_records
            WHERE configuration_version = 'v1'
              AND suspended_reason IS NOT NULL
              AND suspended_since_ms IS NOT NULL
              AND suspended_until_ms > ?
            ORDER BY root_id, domain`,
        )
        .all(nowMs) as unknown as BuildBreakerRow[];
      for (const row of rows) {
        const deathCount = Number(row.zero_credit_deaths) + Number(row.credited_deaths);
        const ageS = Math.floor(Math.max(0, nowMs - Number(row.suspended_since_ms)) / 1_000);
        suspensions.push({
          root: row.root_id,
          domain: row.domain,
          reason: row.suspended_reason,
          deathCount,
          ageS,
          fingerprint: row.corpus_fingerprint,
        });
      }
    } catch {
      // A concurrent first build may leave a database path visible before its
      // schema is committed. Ignore that one file and preserve the rest of the
      // diagnostic report rather than reporting a fabricated clean state.
    } finally {
      database?.close();
    }
  }
  return suspensions;
}

export function formatBuildBreakerSuspension(suspension: BuildBreakerSuspension): string {
  return [
    `  build suspended: root=${suspension.root}`,
    `domain=${suspension.domain}`,
    `deaths=${suspension.deathCount}`,
    `age_s=${suspension.ageS}`,
    `reason=${suspension.reason};`,
    `reset with \`${DOCTOR_BUILD_BREAKER_RESET_COMMAND} --root ${suspension.root} --domain ${suspension.domain} --fingerprint ${suspension.fingerprint}\``,
  ].join(" ");
}

export interface BuildBreakerResetTarget {
  root: string;
  domain: string;
  fingerprint: string;
}

/** Reset one explicit durable breaker tuple without touching sibling roots or domains. */
export function resetBuildBreakerSuspension(
  storageRoot: string,
  target: BuildBreakerResetTarget,
): number {
  let reset = 0;
  for (const databasePath of buildBreakerDatabases(storageRoot)) {
    let database: DatabaseSync | undefined;
    try {
      database = new (loadDatabaseSync())(databasePath);
      const result = database
        .prepare(
          `UPDATE breaker_records
              SET zero_credit_deaths = 0,
                  credited_deaths = 0,
                  in_build_burn_ms = 0,
                  suspended_reason = NULL,
                  suspended_since_ms = NULL,
                  suspended_until_ms = NULL
            WHERE root_id = ?
              AND domain = ?
              AND corpus_fingerprint = ?`,
        )
        .run(target.root, target.domain, target.fingerprint) as { changes?: number };
      reset += Number(result.changes ?? 0);
    } catch {
      // Doctor should remain able to reset records in other roots when one
      // cache was removed or is locked by an active process.
    } finally {
      database?.close();
    }
  }
  return reset;
}
