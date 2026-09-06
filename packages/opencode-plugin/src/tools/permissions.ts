import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { resolveAftStorageRoot } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";

import { sendIgnoredMessage } from "../shared/ignored-message.js";
import type { PluginContext } from "../types.js";
import { expandTilde, projectRootFor } from "./_shared.js";

const UNSUPPORTED_ASK_HOST =
  "AFT requires OpenCode 1.15.5 or newer for permission asks; please upgrade OpenCode";

const RULE_DENIAL_MESSAGE_PREFIX =
  "The user has specified a rule which prevents you from using this specific tool call. Here are some of the relevant rules ";
const USER_REJECTION_MESSAGE_PREFIX =
  "The user rejected permission to use this specific tool call.";
const USER_FEEDBACK_MESSAGE_PREFIX = `${USER_REJECTION_MESSAGE_PREFIX} with the following feedback:`;

export type PermissionAskFailureKind = "rule_denied" | "user_rejected" | "feedback";

export type PermissionAskFailure = {
  kind: PermissionAskFailureKind;
  message: string;
};

function stringProperty(value: unknown, key: string): string | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const property = (value as Record<string, unknown>)[key];
  return typeof property === "string" ? property : undefined;
}

function permissionErrorMessage(error: unknown, fallbackMessage: string): string {
  if (error instanceof Error && error.message) return error.message;
  const message = stringProperty(error, "message");
  return message || fallbackMessage;
}

/**
 * OpenCode's tagged permission errors have stable `_tag` values even when their
 * display wording changes. The constructor names cover hosts that expose the
 * class without the Effect tag; message prefixes are only a compatibility
 * fallback for older wrappers that erased both forms of type information.
 */
export function classifyPermissionError(
  error: unknown,
  fallbackMessage = "Permission denied.",
): PermissionAskFailure {
  const tag = stringProperty(error, "_tag");
  const errorConstructor =
    typeof error === "object" && error !== null
      ? (error as { constructor?: unknown }).constructor
      : undefined;
  const constructorName =
    typeof errorConstructor === "function" && typeof errorConstructor.name === "string"
      ? errorConstructor.name
      : undefined;
  const typeNames = new Set([tag, constructorName]);

  if (typeNames.has("PermissionDeniedError") || typeNames.has("DeniedError")) {
    return { kind: "rule_denied", message: permissionErrorMessage(error, fallbackMessage) };
  }
  if (typeNames.has("PermissionRejectedError") || typeNames.has("RejectedError")) {
    return { kind: "user_rejected", message: permissionErrorMessage(error, fallbackMessage) };
  }
  if (typeNames.has("PermissionCorrectedError") || typeNames.has("CorrectedError")) {
    return { kind: "feedback", message: permissionErrorMessage(error, fallbackMessage) };
  }

  const message = permissionErrorMessage(error, fallbackMessage);
  if (message.startsWith(RULE_DENIAL_MESSAGE_PREFIX)) {
    return { kind: "rule_denied", message };
  }
  if (message.startsWith(USER_FEEDBACK_MESSAGE_PREFIX)) {
    return { kind: "feedback", message };
  }
  if (message.startsWith(USER_REJECTION_MESSAGE_PREFIX)) {
    return { kind: "user_rejected", message };
  }
  return { kind: "feedback", message };
}

export function permissionRuleDenial(permission: string, root?: string): string {
  return (
    `permission_denied: ${permission}${root ? ` for ${root}` : ""} is denied by this session's ` +
    "permission rules (not a user choice)"
  );
}

/**
 * Throttle for the user-facing "restrict_to_project_root blocked this" panel.
 * An agent that probes several external paths shouldn't spawn a panel per path;
 * we surface the notice at most once per session per 5 minutes. Keyed by
 * sessionID. Module-level is acceptable here: this is best-effort UI-noise
 * suppression, not correctness — a duplicate plugin load worst-cases to one
 * extra panel, never a missed block (the block + agent denial are independent
 * of this map).
 */
const RESTRICT_NOTICE_THROTTLE_MS = 5 * 60 * 1000;
const restrictNoticeLastSentAt = new Map<string, number>();
const aftSearchExternalDecisionCache = new Map<
  string,
  AftSearchExternalPermissionDenial | undefined
>();
const aftSearchExternalPendingAsks = new Map<
  string,
  Promise<AftSearchExternalPermissionDenial | undefined>
>();
const POSIX_SYSTEM_TEMP_ROOTS = ["/tmp", "/var/tmp", "/private/tmp", "/private/var/tmp"];
const MACOS_SYSTEM_TEMP_ROOTS = ["/var/folders", "/private/var/folders"];

export type AftSearchExternalPermissionDenial = PermissionAskFailure & {
  permission: "aft_search_external";
  root: string;
};

function restrictNoticeWording(target: string): string {
  return (
    `AFT blocked access to a path outside the project:\n  ${target}\n` +
    "`restrict_to_project_root` is enabled (full isolation), so AFT does not access paths " +
    "outside the project root. To allow external paths, set `restrict_to_project_root: false` " +
    "in your aft.jsonc."
  );
}

/**
 * Fire the throttled restriction notice to the user (ignored panel, no agent
 * turn). Best-effort: never throws into the caller's tool path.
 */
function notifyRestrictBlocked(ctx: PluginContext, context: ToolContext, target: string): void {
  const sessionID = context.sessionID;
  if (!sessionID) return;
  const now = Date.now();
  const last = restrictNoticeLastSentAt.get(sessionID);
  if (last !== undefined && now - last < RESTRICT_NOTICE_THROTTLE_MS) return;
  restrictNoticeLastSentAt.set(sessionID, now);
  void sendIgnoredMessage(ctx.client, sessionID, restrictNoticeWording(target)).catch(() => {
    // UI-only notice; a delivery failure must not affect the block itself.
  });
}

/** Agent-facing denial returned when restrict_to_project_root blocks a path. */
function restrictDenialMessage(target: string): string {
  return (
    `Blocked: '${target}' is outside the project root and restrict_to_project_root is enabled ` +
    "(AFT full isolation). Not overridable per-call; set restrict_to_project_root: false in " +
    "aft.jsonc to allow external paths."
  );
}

/**
 * Execute a `ctx.ask(...)` result.
 *
 * As of `@opencode-ai/plugin@1.15.5`, `ask()` returns `Promise<void>` again
 * (it briefly returned `Effect.Effect<void>` in 1.14.x–1.15.4; the Promise
 * shape is what the SDK originally used and what AFT supports today).
 *
 * On deny, the Promise rejects with `DeniedError`, `RejectedError`, or
 * `CorrectedError`, so callers can rely on a normal `try/catch` to detect the
 * host's rule, user, or feedback outcome. This helper
 * stays as a single chokepoint so that if the SDK ever changes its return
 * shape again, only this function needs to be touched.
 */
export async function runAsk(maybe: Promise<void>): Promise<void> {
  await maybe;
}

export function resolveAbsolutePath(context: ToolContext, target: string): string {
  const expanded = expandTilde(target);
  return path.isAbsolute(expanded) ? expanded : path.resolve(projectRootFor(context), expanded);
}

export function permissionPath(context: ToolContext, target: string): string {
  const projectRoot = path.resolve(projectRootFor(context));
  const absolutePath = path.resolve(resolveAbsolutePath(context, target));
  if (projectRoot === path.parse(projectRoot).root) return absolutePath;

  const relativePath = path.relative(projectRoot, absolutePath);
  if (
    relativePath !== "" &&
    relativePath !== ".." &&
    !relativePath.startsWith(`..${path.sep}`) &&
    !path.isAbsolute(relativePath)
  ) {
    return relativePath;
  }

  return relativePath === "" ? "." : absolutePath;
}

export function resolveRelativePattern(context: ToolContext, target: string): string {
  return permissionPath(context, target);
}

export function resolveRelativePatternFromAbsolute(
  context: ToolContext,
  absolutePath: string,
): string {
  return permissionPath(context, absolutePath);
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
    const failure = classifyPermissionError(error, "Permission denied.");
    return failure.kind === "rule_denied" ? permissionRuleDenial("edit") : failure.message;
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

function systemTempRoots(): string[] {
  const roots = [tmpdir()];
  if (process.platform !== "win32") roots.push(...POSIX_SYSTEM_TEMP_ROOTS);
  if (process.platform === "darwin") roots.push(...MACOS_SYSTEM_TEMP_ROOTS);
  return roots;
}

function isSystemTempPath(target: string): boolean {
  const normalizedTarget = normalizePath(target);
  return systemTempRoots().some((root) => containsPath(normalizePath(root), normalizedTarget));
}

/**
 * Whether a path has the shape of a bash task artifact under the AFT storage
 * root: <storage>/<harness>/bash-tasks/<task-dir>/<artifact-file>.
 *
 * Truncated bash output points agents at these files, and they live outside
 * every project by design. An external-directory prompt here stalls
 * unattended sessions for a read the server validates anyway: Rust's
 * validate_read_path only serves the file when the requesting session OWNS
 * the task (BgTaskRegistry::is_session_owned_artifact_path) and never allows
 * writes. The shape check is deliberately narrow — exactly one path segment
 * per level below bash-tasks — so nothing else under the storage root is
 * exempted.
 */
function isBashTaskArtifactPath(target: string): boolean {
  const storageRoot = normalizePath(resolveAftStorageRoot());
  const normalizedTarget = normalizePath(target);
  if (!containsPath(storageRoot, normalizedTarget)) return false;
  const relative = normalizedTarget.slice(storageRoot.length).replace(/^[/\\]+/, "");
  const segments = relative.split(/[/\\]+/);
  // <harness>/bash-tasks/<task-dir>/<artifact-file>
  return segments.length === 4 && segments[1] === "bash-tasks";
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
 * Resolve symlinks before containsPath() comparisons on every platform.
 *
 * Existing targets are canonicalized directly. For not-yet-created write
 * targets, walk upward until an existing parent can be realpath'd, then rejoin
 * the missing tail; this catches writes through a symlinked parent directory.
 * Windows drive/path normalization is applied before realpath so drive-form
 * variants still compare consistently. This helper is total: permission checks
 * must degrade to a lexical absolute path instead of throwing.
 */
function normalizePath(p: string): string {
  const resolved = path.resolve(windowsPath(p));
  try {
    return fs.realpathSync.native(resolved);
  } catch {
    return normalizeNearestExistingParent(resolved);
  }
}

function normalizeNearestExistingParent(resolved: string): string {
  const missingTail: string[] = [];
  let current = resolved;

  while (true) {
    try {
      const realParent = fs.realpathSync.native(current);
      return missingTail.length === 0
        ? realParent
        : path.join(realParent, ...missingTail.reverse());
    } catch {
      const parent = path.dirname(current);
      if (parent === current) return resolved;
      missingTail.push(path.basename(current));
      current = parent;
    }
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
 * Non-Windows callers build patterns from an already-canonical parent path.
 */
function normalizePathPattern(p: string): string {
  if (process.platform !== "win32") return p;
  if (p === "*" || p === "**") return p;
  const match = p.match(/^(.*)[\\/](\*{1,2})$/);
  if (!match) return normalizePath(p);
  const dir = /^[A-Za-z]:$/.test(match[1]) ? `${match[1]}\\` : match[1];
  return path.join(normalizePath(dir), match[2]);
}

export const _permissionsInternalsForTest = {
  classifyPermissionError,
  containsPath,
  isSystemTempPath,
  normalizePathPattern,
};

/**
 * Trigger OpenCode's host-side `external_directory` permission check when the
 * target path falls outside the current project's directory and worktree.
 * Mirrors `opencode/src/tool/external-directory.ts::assertExternalDirectoryEffect`.
 *
 * Why this exists: AFT hoisted tools previously only called `permission: "edit"`,
 * which bypassed OpenCode's separate `external_directory` rule (default `ask`).
 * This helper keeps that rule active for ordinary out-of-root paths while
 * suppressing asks for system temp directories, which are unstable world-writable
 * scratch space.
 *
 * Returns `undefined` on allow (or when target is inside project), or a
 * denial message string on deny so callers can wrap with
 * `permissionDeniedResponse(...)`.
 *
 * Always call this BEFORE the regular `askEditPermission` so any required
 * external-directory prompt appears before the edit/read prompt.
 */
export async function assertExternalDirectoryPermission(
  ctx: PluginContext,
  context: ToolContext,
  target: string,
  options?: { kind?: "file" | "directory"; serverValidatedRead?: boolean },
): Promise<string | undefined> {
  if (!target) return undefined;

  const resolved = resolveAbsolutePath(context, target);
  // `tool_call` trusts this plugin-side permission decision. The Rust default
  // validator may keep lexical path strings, so canonicalize containment here
  // and assert the ask decision rather than plugin/server string equality.
  const absoluteTarget = normalizePath(resolved);

  const root = projectRootFor(context);
  const directory = root ? normalizePath(root) : root;
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

  // restrict_to_project_root is AFT's full-isolation knob — deliberately NOT
  // conflated with OpenCode's external_directory permission. When it's on, an
  // out-of-root path is hard-blocked at the plugin layer: we do NOT bubble an
  // external_directory prompt (a grant could never override the Rust-side
  // boundary anyway — that produced the issue #125 "approved but still fails"
  // footgun). Instead the agent gets a clear denial and the user gets a
  // throttled informational panel explaining the restriction.
  if (ctx.config.restrict_to_project_root === true) {
    // A session-owned bash artifact lives outside the project by design. The
    // plugin has no access to Rust's task registry, so read calls cross this
    // boundary and let Rust apply its exact, session-scoped artifact check.
    if (options?.serverValidatedRead === true) return undefined;
    notifyRestrictBlocked(ctx, context, absoluteTarget);
    return restrictDenialMessage(absoluteTarget);
  }

  if (isSystemTempPath(absoluteTarget)) return undefined;

  // Bash task artifacts under the AFT storage root: skip the external
  // prompt for server-validated reads — Rust only serves them to the owning
  // session, so the prompt protected nothing and stalled unattended runs
  // whenever a truncation footer pointed the agent at its own task output.
  if (options?.serverValidatedRead === true && isBashTaskArtifactPath(absoluteTarget)) {
    return undefined;
  }

  if (typeof context.ask !== "function") return UNSUPPORTED_ASK_HOST;

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
    const failure = classifyPermissionError(error, "Permission denied (external directory).");
    return failure.kind === "rule_denied"
      ? permissionRuleDenial("external_directory", parentDir)
      : failure.message;
  }
}

function gitRootForNearestExistingParent(resolved: string): string | undefined {
  const nearest = normalizeNearestExistingParent(resolved);
  let cwd = nearest;
  try {
    if (fs.statSync(nearest).isFile()) cwd = path.dirname(nearest);
  } catch {
    // `normalizeNearestExistingParent` should return an existing path when it
    // can, but keep the permission check best-effort and let Rust report
    // `not_a_git_root` for unusual races.
  }
  try {
    const out = execFileSync("git", ["rev-parse", "--show-toplevel"], {
      cwd,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
    return out ? normalizePath(out) : undefined;
  } catch {
    return undefined;
  }
}

/**
 * Ask OpenCode for permission to expose indexed content from a different git root.
 * This deliberately uses a tool-specific permission id rather than
 * `external_directory`: a user may allow ordinary path reads while still wanting
 * a separate audit point for search results produced from borrowed indexes.
 *
 * A denial keeps its classification and root so aft_search can safely fall back
 * to the current project's index only for session-rule denials. The result is
 * cached with the same session/root key as the old denial string, so a rule
 * decision cannot change halfway through a session.
 */
export async function assertAftSearchExternalPermission(
  ctx: PluginContext,
  context: ToolContext,
  target: string,
): Promise<AftSearchExternalPermissionDenial | undefined> {
  if (!target) return undefined;

  const resolved = resolveAbsolutePath(context, target);
  const absoluteTarget = normalizePath(resolved);
  const externalRoot =
    gitRootForNearestExistingParent(resolved) ?? normalizeNearestExistingParent(resolved);

  const root = projectRootFor(context);
  const directory = root ? normalizePath(root) : root;
  const rawWorktree = (context as { worktree?: string }).worktree;
  const worktree = rawWorktree && rawWorktree !== "/" ? normalizePath(rawWorktree) : rawWorktree;

  if (directory && containsPath(directory, externalRoot)) return undefined;
  if (
    worktree &&
    worktree !== "/" &&
    worktree !== directory &&
    containsPath(worktree, externalRoot)
  ) {
    return undefined;
  }

  if (ctx.config.restrict_to_project_root === true) {
    notifyRestrictBlocked(ctx, context, externalRoot);
    return {
      kind: "feedback",
      message: restrictDenialMessage(externalRoot),
      permission: "aft_search_external",
      root: externalRoot,
    };
  }

  if (typeof context.ask !== "function") {
    return {
      kind: "feedback",
      message: UNSUPPORTED_ASK_HOST,
      permission: "aft_search_external",
      root: externalRoot,
    };
  }

  const sessionKey = (context as { sessionID?: string }).sessionID ?? "unknown-session";
  const cacheKey = `${sessionKey}\0${externalRoot}`;
  if (aftSearchExternalDecisionCache.has(cacheKey)) {
    return aftSearchExternalDecisionCache.get(cacheKey);
  }
  const pending = aftSearchExternalPendingAsks.get(cacheKey);
  if (pending) return pending;

  const rawGlob =
    process.platform === "win32"
      ? normalizePathPattern(path.join(externalRoot, "*"))
      : path.join(externalRoot, "*").replaceAll("\\", "/");
  const askPromise = (async () => {
    try {
      await runAsk(
        context.ask({
          permission: "aft_search_external",
          patterns: [rawGlob],
          always: [rawGlob],
          metadata: {
            filepath: absoluteTarget,
            root: externalRoot,
          },
        }),
      );
      aftSearchExternalDecisionCache.set(cacheKey, undefined);
      return undefined;
    } catch (error) {
      const failure = classifyPermissionError(error, "Permission denied (aft_search_external).");
      const denial: AftSearchExternalPermissionDenial = {
        kind: failure.kind,
        message:
          failure.kind === "rule_denied"
            ? permissionRuleDenial("aft_search_external", externalRoot)
            : failure.message,
        permission: "aft_search_external",
        root: externalRoot,
      };
      aftSearchExternalDecisionCache.set(cacheKey, denial);
      return denial;
    } finally {
      aftSearchExternalPendingAsks.delete(cacheKey);
    }
  })();
  aftSearchExternalPendingAsks.set(cacheKey, askPromise);
  return askPromise;
}

type SearchPermissionId = "grep" | "aft_search" | "aft_search_external";

/**
 * Trigger an OpenCode host-side search permission check using grep-compatible
 * pattern, always, and metadata fields. The permission id is a parameter so
 * aft_search can be governed independently from the raw grep tool.
 */
async function askSearchPatternPermission(
  context: ToolContext,
  permission: SearchPermissionId,
  pattern: string,
  metadata: { path?: string; include?: string } = {},
): Promise<string | undefined> {
  if (typeof context.ask !== "function") return UNSUPPORTED_ASK_HOST;
  try {
    await runAsk(
      context.ask({
        permission,
        patterns: [pattern],
        always: ["*"],
        metadata: { pattern, ...metadata },
      }),
    );
    return undefined;
  } catch (error) {
    const failure = classifyPermissionError(error, `Permission denied (${permission}).`);
    return failure.kind === "rule_denied" ? permissionRuleDenial(permission) : failure.message;
  }
}

/**
 * Trigger the host OpenCode permission check for grep.
 *
 * Uses the same request shape as OpenCode's built-in grep tool so a user's
 * `"permission": { "grep": { "*": "ask" } }` (or "deny") setting behaves
 * the same for the plugin-provided grep tool and OpenCode's native grep tool.
 */
export async function askGrepPermission(
  context: ToolContext,
  pattern: string,
  metadata: { path?: string; include?: string } = {},
): Promise<string | undefined> {
  return askSearchPatternPermission(context, "grep", pattern, metadata);
}

/**
 * Trigger the host OpenCode permission check for aft_search.
 *
 * Passes the same pattern value and metadata as askGrepPermission but registers
 * under the `aft_search` permission id so rules targeting only `grep` do not
 * apply.
 */
export async function askSearchPermission(
  context: ToolContext,
  pattern: string,
  metadata: { path?: string; include?: string } = {},
): Promise<string | undefined> {
  return askSearchPatternPermission(context, "aft_search", pattern, metadata);
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
    const failure = classifyPermissionError(error, "Permission denied (glob).");
    return failure.kind === "rule_denied" ? permissionRuleDenial("glob") : failure.message;
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
