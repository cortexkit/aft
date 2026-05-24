import * as fs from "node:fs";
import * as path from "node:path";
import type { ToolContext } from "@opencode-ai/plugin";

const UNSUPPORTED_ASK_HOST =
  "AFT requires OpenCode 1.15.5 or newer for permission asks; please upgrade OpenCode";

/**
 * Execute a `ctx.ask(...)` result.
 *
 * As of `@opencode-ai/plugin@1.15.5`, `ask()` returns `Promise<void>` again
 * (it briefly returned `Effect.Effect<void>` in 1.14.x–1.15.4; the Promise
 * shape is what the SDK originally used and what AFT supports today).
 *
 * On deny, the Promise rejects with `DeniedError` / `RejectedError`, so
 * callers can rely on a normal `try/catch` to detect denial. This helper
 * stays as a single chokepoint so that if the SDK ever changes its return
 * shape again, only this function needs to be touched.
 */
export async function runAsk(maybe: Promise<void>): Promise<void> {
  await maybe;
}

export function resolveAbsolutePath(context: ToolContext, target: string): string {
  return path.isAbsolute(target) ? target : path.resolve(context.directory, target);
}

export function resolveRelativePattern(context: ToolContext, target: string): string {
  return path.relative(context.worktree, resolveAbsolutePath(context, target)) || ".";
}

export function resolveRelativePatterns(context: ToolContext, targets: string[]): string[] {
  const seen = new Set<string>();
  const patterns: string[] = [];

  for (const target of targets) {
    if (!target) continue;
    const pattern = resolveRelativePattern(context, target);
    if (seen.has(pattern)) continue;
    seen.add(pattern);
    patterns.push(pattern);
  }

  return patterns;
}

export function workspacePattern(_context: ToolContext): string {
  return ".";
}

export async function askEditPermission(
  context: ToolContext,
  patterns: string[],
  metadata: Record<string, unknown> = {},
): Promise<string | undefined> {
  if (typeof context.ask !== "function") return UNSUPPORTED_ASK_HOST;
  try {
    await runAsk(
      context.ask({
        permission: "edit",
        patterns: patterns.length > 0 ? patterns : [workspacePattern(context)],
        always: ["*"],
        metadata,
      }),
    );
    return undefined;
  } catch (error) {
    if (error instanceof Error && error.message) {
      return error.message;
    }
    return "Permission denied.";
  }
}

/**
 * Check if `child` is inside `parent`. Mirrors `AppFileSystem.contains` in
 * opencode core (uses `path.relative` and ensures it doesn't start with `..`).
 */
function containsPath(parent: string, child: string): boolean {
  if (!parent) return false;
  const rel = path.relative(parent, child);
  return rel === "" || (!rel.startsWith("..") && !path.isAbsolute(rel));
}

/**
 * Convert POSIX-style drive paths to Windows drive paths.
 *
 * Mirrors `AppFileSystem.windowsPath` in opencode core — these forms can
 * leak into our input from Git Bash, Cygwin, and WSL conversions:
 *
 *   `/c/Users/...`         → `C:/Users/...`
 *   `/cygdrive/c/...`      → `C:/...`
 *   `/mnt/c/...`           → `C:/...`
 *
 * No-op on non-Windows.
 */
function windowsPath(p: string): string {
  if (process.platform !== "win32") return p;
  return p
    .replace(/^\/([a-zA-Z]):(?:[\\/]|$)/, (_, drive) => `${drive.toUpperCase()}:/`)
    .replace(/^\/([a-zA-Z])(?:\/|$)/, (_, drive) => `${drive.toUpperCase()}:/`)
    .replace(/^\/cygdrive\/([a-zA-Z])(?:\/|$)/, (_, drive) => `${drive.toUpperCase()}:/`)
    .replace(/^\/mnt\/([a-zA-Z])(?:\/|$)/, (_, drive) => `${drive.toUpperCase()}:/`);
}

/**
 * Normalize a path so containsPath() comparisons and external_directory
 * glob construction work consistently on Windows.
 *
 * Mirrors `AppFileSystem.normalizePath` in opencode core: on Windows,
 * applies POSIX→Windows drive translation, resolves to absolute, then
 * `realpathSync.native` to follow symlinks and canonicalize the drive
 * letter case. Falls back to the resolved path when the target doesn't
 * exist (writes to new files have to ask permission BEFORE creating).
 *
 * No-op on non-Windows so macOS/Linux behavior is unchanged.
 */
function normalizePath(p: string): string {
  if (process.platform !== "win32") return p;
  const resolved = path.resolve(windowsPath(p));
  try {
    return fs.realpathSync.native(resolved);
  } catch {
    return resolved;
  }
}

/**
 * Normalize a path pattern (which may end in `*`) for the same reasons
 * normalizePath() exists, but without trying to realpath a pattern that
 * doesn't correspond to a real entry.
 *
 * Mirrors `AppFileSystem.normalizePathPattern` in opencode core.
 *
 *   `*`                 → `*`
 *   `~/projects/*`      → `~/projects/*`  (`~` is expanded by opencode's matcher)
 *   `C:\some\dir\*`     → `C:\some\dir\*` (drive case canonicalized via realpath of the dir part)
 *
 * No-op on non-Windows.
 */
function normalizePathPattern(p: string): string {
  if (process.platform !== "win32") return p;
  if (p === "*" || p === "**") return p;
  const match = p.match(/^(.*)[\\/](\*{1,2})$/);
  if (!match) return normalizePath(p);
  const dir = /^[A-Za-z]:$/.test(match[1]) ? `${match[1]}\\` : match[1];
  return path.join(normalizePath(dir), match[2]);
}

export const _permissionsInternalsForTest = { containsPath, normalizePathPattern };

/**
 * Trigger OpenCode's host-side `external_directory` permission check when the
 * target path falls outside the current project's directory and worktree.
 * Mirrors `opencode/src/tool/external-directory.ts::assertExternalDirectoryEffect`.
 *
 * Why this exists: AFT hoisted tools previously only called `permission: "edit"`,
 * which bypassed OpenCode's separate `external_directory` rule (default `ask`).
 * That meant `/tmp/anything` writes routed through AFT silently bypassed the
 * prompt OpenCode native `write`/`edit`/`apply_patch`/`read` show. This helper
 * closes that gap so AFT's hoisted surface matches native behavior.
 *
 * Returns `undefined` on allow (or when target is inside project), or a
 * denial message string on deny so callers can wrap with
 * `permissionDeniedResponse(...)`.
 *
 * Always call this BEFORE the regular `askEditPermission` so the user sees the
 * external-directory prompt first (matching opencode native ordering). When the
 * external-directory rule is `allow` (e.g. for `${os.tmpdir()}/opencode/*`), the
 * call short-circuits and the regular permission flow continues normally.
 */
export async function assertExternalDirectoryPermission(
  context: ToolContext,
  target: string,
  options?: { kind?: "file" | "directory" },
): Promise<string | undefined> {
  if (!target) return undefined;
  if (typeof context.ask !== "function") return UNSUPPORTED_ASK_HOST;

  const resolved = path.isAbsolute(target) ? target : path.resolve(context.directory, target);
  // Windows: realpath + drive-case normalize so containsPath comparisons line
  // up regardless of how the agent typed the path (`C:/...` vs `/c/...` vs
  // `/cygdrive/c/...`). No-op on macOS/Linux.
  const absoluteTarget = normalizePath(resolved);

  const directory = context.directory ? normalizePath(context.directory) : context.directory;
  const rawWorktree = (context as { worktree?: string }).worktree;
  const worktree = rawWorktree && rawWorktree !== "/" ? normalizePath(rawWorktree) : rawWorktree;

  if (directory && containsPath(directory, absoluteTarget)) return undefined;
  // Non-git projects set worktree to "/" which matches ANY absolute path.
  // Match opencode's behavior: skip the worktree check in that case so we
  // still ask for external paths.
  if (
    worktree &&
    worktree !== "/" &&
    worktree !== directory &&
    containsPath(worktree, absoluteTarget)
  ) {
    return undefined;
  }

  const kind = options?.kind ?? "file";
  const parentDir = kind === "directory" ? absoluteTarget : path.dirname(absoluteTarget);
  const rawGlob =
    process.platform === "win32"
      ? normalizePathPattern(path.join(parentDir, "*"))
      : path.join(parentDir, "*").replaceAll("\\", "/");

  try {
    await runAsk(
      context.ask({
        permission: "external_directory",
        patterns: [rawGlob],
        always: [rawGlob],
        metadata: {
          filepath: absoluteTarget,
          parentDir,
        },
      }),
    );
    return undefined;
  } catch (error) {
    if (error instanceof Error && error.message) {
      return error.message;
    }
    return "Permission denied (external directory).";
  }
}

/**
 * Trigger OpenCode's host-side `grep` permission check.
 *
 * Mirrors `opencode/src/tool/grep.ts` shape exactly so users with
 * `"permission": { "grep": { "*": "ask" } }` (or "deny") see the same
 * prompt regardless of whether they're using AFT's hoisted `grep` or
 * OpenCode's built-in.
 */
export async function askGrepPermission(
  context: ToolContext,
  pattern: string,
  metadata: { path?: string; include?: string } = {},
): Promise<string | undefined> {
  if (typeof context.ask !== "function") return UNSUPPORTED_ASK_HOST;
  try {
    await runAsk(
      context.ask({
        permission: "grep",
        patterns: [pattern],
        always: ["*"],
        metadata: { pattern, ...metadata },
      }),
    );
    return undefined;
  } catch (error) {
    if (error instanceof Error && error.message) {
      return error.message;
    }
    return "Permission denied (grep).";
  }
}

/**
 * Trigger OpenCode's host-side `glob` permission check.
 *
 * Mirrors `opencode/src/tool/glob.ts` shape exactly so users with
 * `"permission": { "glob": { "*": "ask" } }` see the same prompt
 * regardless of which glob tool is used.
 */
export async function askGlobPermission(
  context: ToolContext,
  pattern: string,
  metadata: { path?: string } = {},
): Promise<string | undefined> {
  if (typeof context.ask !== "function") return UNSUPPORTED_ASK_HOST;
  try {
    await runAsk(
      context.ask({
        permission: "glob",
        patterns: [pattern],
        always: ["*"],
        metadata: { pattern, ...metadata },
      }),
    );
    return undefined;
  } catch (error) {
    if (error instanceof Error && error.message) {
      return error.message;
    }
    return "Permission denied (glob).";
  }
}

export function permissionDeniedResponse(message: string): string {
  return JSON.stringify({
    success: false,
    code: "permission_denied",
    message,
    error: message,
  });
}
