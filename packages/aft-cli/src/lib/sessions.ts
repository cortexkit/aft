import { type Dirent, existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import { basename, join } from "node:path";
import type { HarnessAdapter } from "../adapters/types.js";
import { getOmpSessionsRoot } from "./omp-paths.js";

export interface RecentSession {
  id: string;
  title: string;
  lastActivity: number;
}

interface OpenCodeSessionRow {
  id: unknown;
  title: unknown;
  time_updated: unknown;
}

interface SqliteDatabase {
  prepare(sql: string): { all(): OpenCodeSessionRow[] };
  close(): void;
}

const MAX_RECENT_SESSIONS = 5;

export function listRecentSessions(adapter: HarnessAdapter): RecentSession[] {
  try {
    if (adapter.kind === "opencode") return listRecentOpenCodeSessions();
    if (adapter.kind === "pi") return listRecentPiSessions();
    if (adapter.kind === "omp") return listPiSessionsFromDir(getOmpSessionsRoot());
    return [];
  } catch {
    return [];
  }
}

export function mapOpenCodeSessionRows(rows: OpenCodeSessionRow[]): RecentSession[] {
  return rows
    .map((row) => {
      if (typeof row.id !== "string" || row.id.length === 0) return null;
      if (typeof row.title !== "string" || row.title.length === 0) return null;
      const lastActivity =
        typeof row.time_updated === "number" ? row.time_updated : Number(row.time_updated);
      if (!Number.isFinite(lastActivity)) return null;
      return {
        id: row.id,
        title: row.title,
        lastActivity,
      } satisfies RecentSession;
    })
    .filter((session): session is RecentSession => session !== null)
    .sort((a, b) => b.lastActivity - a.lastActivity)
    .slice(0, MAX_RECENT_SESSIONS);
}

function listRecentOpenCodeSessions(): RecentSession[] {
  const dbPath = join(getXdgDataHome(), "opencode", "opencode.db");
  if (!existsSync(dbPath)) return [];

  let db: SqliteDatabase | null = null;
  try {
    const require = createRequire(import.meta.url);
    const sqlite = require("node:sqlite") as {
      DatabaseSync: new (path: string, options: { readOnly: boolean }) => SqliteDatabase;
    };
    db = new sqlite.DatabaseSync(dbPath, { readOnly: true });
    const rows = db
      .prepare("SELECT id, title, time_updated FROM session ORDER BY time_updated DESC LIMIT 5")
      .all();
    return mapOpenCodeSessionRows(rows);
  } catch {
    return [];
  } finally {
    try {
      db?.close();
    } catch {
      // ignore close failures during graceful degradation
    }
  }
}

function getXdgDataHome(): string {
  const xdgDataHome = process.env.XDG_DATA_HOME;
  return xdgDataHome && xdgDataHome.length > 0 ? xdgDataHome : join(homedir(), ".local", "share");
}

function listRecentPiSessions(): RecentSession[] {
  return listPiSessionsFromDir(join(getHomeDir(), ".pi", "agent", "sessions"));
}

function getHomeDir(): string {
  const envHome = process.platform === "win32" ? process.env.USERPROFILE : process.env.HOME;
  return envHome && envHome.length > 0 ? envHome : homedir();
}

export function listPiSessionsFromDir(sessionsDir: string): RecentSession[] {
  try {
    if (!existsSync(sessionsDir)) return [];
    const files = collectJsonlFiles(sessionsDir)
      .map((filePath) => {
        try {
          const stats = statSync(filePath);
          return { filePath, mtimeMs: stats.mtimeMs };
        } catch {
          return null;
        }
      })
      .filter((entry): entry is { filePath: string; mtimeMs: number } => entry !== null)
      .sort((a, b) => b.mtimeMs - a.mtimeMs)
      .slice(0, MAX_RECENT_SESSIONS * 4);

    const sessions: RecentSession[] = [];
    for (const file of files) {
      const parsed = parsePiSessionJsonl(
        readFileSync(file.filePath, "utf8"),
        basename(file.filePath),
      );
      if (!parsed) continue;
      sessions.push({ ...parsed, lastActivity: file.mtimeMs });
      if (sessions.length >= MAX_RECENT_SESSIONS) break;
    }
    return sessions;
  } catch {
    return [];
  }
}

function collectJsonlFiles(root: string): string[] {
  const files: string[] = [];
  const stack = [root];
  while (stack.length > 0) {
    const dir = stack.pop();
    if (!dir) continue;
    let entries: Dirent[];
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) {
        stack.push(path);
      } else if (entry.isFile() && entry.name.endsWith(".jsonl")) {
        files.push(path);
      }
    }
  }
  return files;
}

export function parsePiSessionJsonl(
  jsonl: string,
  fallbackFilename = "",
): Pick<RecentSession, "id" | "title"> | null {
  let id: string | null = extractUuidFromFilename(fallbackFilename);
  let title: string | null = null;

  for (const line of jsonl.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (trimmed.length === 0) continue;
    let value: unknown;
    try {
      value = JSON.parse(trimmed);
    } catch {
      continue;
    }
    if (!value || typeof value !== "object") continue;
    const record = value as Record<string, unknown>;

    if (record.type === "session" && typeof record.id === "string" && record.id.length > 0) {
      id = record.id;
    }

    if (title === null) {
      const maybeTitle = extractPiUserMessageText(record);
      if (maybeTitle) title = truncateTitle(maybeTitle);
    }

    if (id && title) break;
  }

  if (!id) return null;
  return { id, title: title ?? id };
}

function extractPiUserMessageText(record: Record<string, unknown>): string | null {
  if (record.type !== "message") return null;
  const message = record.message;
  if (!message || typeof message !== "object") return null;
  const messageRecord = message as Record<string, unknown>;
  if (messageRecord.role !== "user") return null;
  return extractTextFromContent(messageRecord.content);
}

function extractTextFromContent(content: unknown): string | null {
  if (typeof content === "string") return content.trim() || null;
  if (!Array.isArray(content)) return null;

  const parts = content
    .map((part) => {
      if (typeof part === "string") return part;
      if (!part || typeof part !== "object") return "";
      const partRecord = part as Record<string, unknown>;
      return partRecord.type === "text" && typeof partRecord.text === "string"
        ? partRecord.text
        : "";
    })
    .filter((text) => text.trim().length > 0);

  const joined = parts.join(" ").trim();
  return joined.length > 0 ? joined : null;
}

function extractUuidFromFilename(filename: string): string | null {
  const match = filename.match(
    /([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$/i,
  );
  return match?.[1] ?? null;
}

export function truncateTitle(title: string, maxLength = 60): string {
  const normalized = title.replace(/\s+/g, " ").trim();
  if (normalized.length <= maxLength) return normalized;
  return `${normalized.slice(0, Math.max(0, maxLength - 1))}…`;
}
