import { spawnSync } from "node:child_process";
import { existsSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, resolve, sep } from "node:path";

/**
 * Turn filesystem errors into one readable line for the setup and doctor UI.
 *
 * The case that matters most is a permission error: `sudo npm i -g` for a host
 * (OpenCode) commonly leaves `~/.config/<host>` or `~/.cache` owned by root, and
 * every later write by the user fails with EACCES. A raw Node stack trace tells
 * the user nothing they can act on; this names the path, who owns it, and the
 * exact command that fixes it.
 */

const PERMISSION_CODES = new Set(["EACCES", "EPERM", "EROFS"]);

/** Injectable filesystem facts, so the message can be tested for any owner. */
export interface PermissionFacts {
  /** Owner uid of an existing path, or null when it cannot be read. */
  ownerUid(path: string): number | null;
  /** The uid this process runs as, or null where uids do not exist (Windows). */
  currentUid(): number | null;
  /** A user name for a uid, or null when it cannot be resolved. */
  userName(uid: number): string | null;
  exists(path: string): boolean;
  home(): string;
}

const defaultFacts: PermissionFacts = {
  ownerUid(path) {
    try {
      return statSync(path).uid;
    } catch {
      return null;
    }
  },
  currentUid() {
    return typeof process.getuid === "function" ? process.getuid() : null;
  },
  userName(uid) {
    if (uid === 0) return "root";
    try {
      const result = spawnSync("id", ["-nu", String(uid)], { encoding: "utf8", timeout: 2_000 });
      const name = result.status === 0 ? result.stdout.trim() : "";
      return name.length > 0 ? name : null;
    } catch {
      return null;
    }
  },
  exists(path) {
    try {
      return existsSync(path);
    } catch {
      return false;
    }
  },
  home: () => homedir(),
};

function errorCode(error: unknown): string | undefined {
  if (typeof error === "object" && error !== null && "code" in error) {
    const code = (error as { code?: unknown }).code;
    return typeof code === "string" ? code : undefined;
  }
  return undefined;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** True for EACCES/EPERM/EROFS errors, or text carrying one of those codes. */
export function isPermissionError(error: unknown): boolean {
  const code = errorCode(error);
  if (code && PERMISSION_CODES.has(code)) return true;
  return /\b(EACCES|EPERM|EROFS)\b|permission denied|os error 13/i.test(errorMessage(error));
}

/** The path a Node fs error names, from `error.path` or the quoted path in its message. */
export function errorPath(error: unknown): string | null {
  if (typeof error === "object" && error !== null && "path" in error) {
    const path = (error as { path?: unknown }).path;
    if (typeof path === "string" && path.length > 0) return path;
  }
  const message = errorMessage(error);
  // Node: "EACCES: permission denied, open '/path'".
  const quoted = message.match(/'([^']+)'/);
  if (quoted?.[1]) return quoted[1];
  // The native binary: "could not write /path: Permission denied (os error 13)".
  const native = message.match(
    /\b(?:write|update|create|open|read|mkdir|rename)\s+((?:[A-Za-z]:)?[\\/][^:\n]+?):\s/i,
  );
  return native?.[1] ?? null;
}

/** Show paths under the home directory as `~/…`, which is how users type them. */
export function tildePath(path: string, home: string = homedir()): string {
  if (path === home) return "~";
  return path.startsWith(home + sep) ? `~${path.slice(home.length)}` : path;
}

/** Quote a path for a POSIX shell only when it needs it; keep `~/` expandable. */
function shellPath(path: string): string {
  if (/^[\w@%+=:,./~-]+$/.test(path)) return path;
  if (path.startsWith("~/")) return `~/"${path.slice(2).replace(/(["\\$`])/g, "\\$1")}"`;
  return `'${path.replace(/'/g, "'\\''")}'`;
}

/**
 * Describe a failed write to `target`.
 *
 * The owner shown is the nearest existing path (the file itself, or the first
 * directory above a file that does not exist yet). When another user owns it,
 * the fix widens to the highest directory below home that the same foreign
 * owner holds, because `sudo npm i -g` usually takes the whole tree
 * (`~/.config/opencode`, not just one file inside it).
 */
export function describePermissionProblem(
  target: string,
  facts: PermissionFacts = defaultFacts,
): string {
  const home = resolve(facts.home());
  let existing = resolve(target);
  while (!facts.exists(existing) && dirname(existing) !== existing) existing = dirname(existing);

  const ownerUid = facts.ownerUid(existing);
  const me = facts.currentUid();
  const shownTarget = tildePath(target, home);

  if (ownerUid === null || me === null) {
    return `Cannot write ${shownTarget}: permission denied. Give your user write access to ${tildePath(existing, home)} and rerun.`;
  }

  const ownerName = facts.userName(ownerUid) ?? `uid ${ownerUid}`;
  if (ownerUid === me) {
    const fix = `chmod u+w ${shellPath(tildePath(existing, home))}`;
    return `Cannot write ${shownTarget}: permission denied. ${tildePath(existing, home)} is owned by you (${ownerName}) but is not writable. Fix: ${fix}`;
  }

  let widest = existing;
  while (true) {
    const parent = dirname(widest);
    if (parent === widest || parent === home || !parent.startsWith(home + sep)) break;
    if (facts.ownerUid(parent) !== ownerUid) break;
    widest = parent;
  }
  const fix = `sudo chown -R $(whoami) ${shellPath(tildePath(widest, home))}`;
  return `Cannot write ${shownTarget}: permission denied. ${tildePath(existing, home)} is owned by ${ownerName}, not you. Fix: ${fix}`;
}

/**
 * One line for any filesystem error. Permission errors get the owner and fix
 * command; everything else keeps its own message. `fallbackPath` is used when
 * the error does not name a path itself.
 */
export function formatFsError(
  error: unknown,
  fallbackPath?: string,
  facts: PermissionFacts = defaultFacts,
): string {
  if (isPermissionError(error)) {
    const path = errorPath(error) ?? fallbackPath;
    if (path) return describePermissionProblem(path, facts);
  }
  return errorMessage(error);
}
