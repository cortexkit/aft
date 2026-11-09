/**
 * Approval tiers for AFT tools on OMP (oh-my-pi).
 *
 * OMP gates every tool call on its approval mode (`tools.approvalMode`:
 * `always-ask` | `write` | `yolo`, forced to `yolo` by `--auto-approve`). A
 * tool declares its capability tier through an `approval` field; a tool that
 * declares none is treated as `exec`, so in `write` mode OMP would prompt for
 * every AFT call, reads included (oh-my-pi
 * `packages/coding-agent/src/tools/approval.ts`, `normalizeDecision` and
 * `resolveApproval`).
 *
 * OMP does not tell an extension tool which approval mode the session runs in,
 * so AFT never raises its own outside-access prompt on OMP. Instead the tier is
 * a function of the call's arguments: a call naming a path outside the
 * session's project reports `exec`, so OMP's own mode decides whether it is
 * asked about (prompted in `write` and `always-ask`, never in `yolo`). Because
 * AFT approves the engine's outside-access asks itself on OMP, this tier is the
 * only place OMP's mode can decide outside access, so the check follows
 * symbolic links, reads patch targets, and reports `exec` when a path cannot
 * be resolved or a patch's targets cannot be read. (Arguments the tool's own
 * preparation rejects keep the tool's tier: that call fails before running.)
 * The engine still enforces its floors and `restrict_to_project_root`.
 *
 * Upstream Pi has no approval gate and gets no `approval` field.
 */

import { realpathSync } from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { decodeFileUrl, relativePathEscapesRoot } from "@cortexkit/aft-bridge";

/** OMP's capability tiers, least to most privileged. */
export type OmpToolTier = "read" | "write" | "exec";

/**
 * The tier of each AFT tool when every path it names is inside the project.
 * A tool missing from this table declares nothing, and OMP treats it as `exec`.
 */
export const OMP_TOOL_TIERS: Readonly<Record<string, OmpToolTier>> = {
  // Read-only: they read files, indexes or background-task state.
  read: "read",
  grep: "read",
  glob: "read",
  aft_search: "read",
  aft_outline: "read",
  aft_zoom: "read",
  aft_callgraph: "read",
  aft_inspect: "read",
  // Lists merge-conflict regions; it changes nothing.
  aft_conflicts: "read",
  // Searches only; the replacing twin is `ast_grep_replace` below.
  ast_grep_search: "read",
  bash_status: "read",
  bash_watch: "read",
  // Change files: edits, moves and deletes, imports, and undo/restore, which
  // write files back from their backups.
  write: "write",
  edit: "write",
  apply_patch: "write",
  aft_delete: "write",
  aft_move: "write",
  aft_import: "write",
  aft_safety: "write",
  ast_grep_replace: "write",
  // Run commands, or act on running ones.
  bash: "exec",
  powershell: "exec",
  bash_write: "exec",
  bash_kill: "exec",
};

const TIER_RANK: Readonly<Record<OmpToolTier, number>> = { read: 0, write: 1, exec: 2 };

/**
 * Argument keys whose string values (or arrays of strings) name filesystem
 * paths. A tool's own `prepareArguments`, which `ompCallTier` runs first,
 * rewrites the older alias spellings it accepts to these keys.
 */
const PATH_KEYS = new Set([
  "path",
  "paths",
  "file",
  "files",
  "target",
  "scope",
  "workdir",
  "destination",
  "toPath",
]);

/** A non-`file:` URL in `scheme://` form (an `https://` page, an `issue://N` reference) names no filesystem path. */
const NON_FILE_URL = /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//;

function expandPath(value: string): string {
  const decoded = decodeFileUrl(value);
  if (decoded === "~") return homedir();
  if (decoded.startsWith("~/") || decoded.startsWith(`~${sep}`))
    return resolve(homedir(), decoded.slice(2));
  return decoded;
}

/**
 * Where `path` really is on disk: its real path when it exists, else the real
 * path of its nearest existing ancestor joined with the components that do
 * not exist yet. Throws for any failure other than not-found (a link loop, a
 * permission error, a file used as a directory).
 */
function realLocation(path: string): string {
  const missing: string[] = [];
  let current = path;
  for (;;) {
    try {
      return missing.length > 0 ? join(realpathSync(current), ...missing) : realpathSync(current);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    }
    const parent = dirname(current);
    if (parent === current) return path;
    missing.unshift(basename(current));
    current = parent;
  }
}

/**
 * Whether `value`, resolved against `root`, lies outside it: as written, or
 * once symbolic links in it (and in `root`) are followed. A path that cannot
 * be resolved for a reason other than not existing counts as outside, so the
 * gate fails toward asking. Synchronous and never throws.
 */
export function namesOutsidePath(root: string, value: string): boolean {
  if (value.length === 0) return false;
  if (!value.startsWith("file:") && NON_FILE_URL.test(value)) return false;
  const expanded = expandPath(value);
  if (expanded.startsWith("file:")) return false;
  const absolute = isAbsolute(expanded) ? expanded : resolve(root, expanded);
  if (relativePathEscapesRoot(relative(root, absolute))) return true;
  try {
    return relativePathEscapesRoot(relative(realLocation(root), realLocation(absolute)));
  } catch {
    return true;
  }
}

function collectPaths(value: unknown, key: string | undefined, out: string[], depth: number): void {
  if (depth > 6 || value === null || value === undefined) return;
  if (typeof value === "string") {
    if (key !== undefined && PATH_KEYS.has(key)) out.push(value);
    return;
  }
  if (Array.isArray(value)) {
    for (const item of value) collectPaths(item, key, out, depth + 1);
    return;
  }
  if (typeof value === "object") {
    for (const [child, item] of Object.entries(value as Record<string, unknown>))
      collectPaths(item, child, out, depth + 1);
  }
}

/** The path-like argument values of one call, nested batches included (to six levels deep). */
export function pathArguments(args: unknown): string[] {
  const out: string[] = [];
  collectPaths(args, undefined, out, 0);
  return out;
}

function unquote(value: string): string {
  const first = value[0];
  return value.length >= 2 && (first === '"' || first === "'") && value.endsWith(first)
    ? value.slice(1, -1)
    : value;
}

/** One `[path#TAG]` section of a hashline patch and the `MV` destinations it names. */
export interface HashlinePatchSection {
  path: string;
  moves: string[];
}

/**
 * The sections of a hashline patch, in patch order: each `[path#TAG]` section
 * header with the `MV` destinations that follow it (grammar: docs/hashline.md).
 * Lines are classified in the engine parser's order (envelope, header,
 * directive, `+` body row), so every path the engine would act on is listed. It
 * extracts targets and does not validate the rest of the grammar, so it may
 * list paths for a patch the engine later rejects. Undefined when a line fits
 * none of those forms or a header or `MV` target is malformed.
 */
export function hashlinePatchSections(patch: string): HashlinePatchSection[] | undefined {
  if (patch.trim().length === 0) return undefined;
  const sections: HashlinePatchSection[] = [];
  const rows = patch.split("\n");
  for (let index = 0; index < rows.length; index++) {
    const raw = rows[index] as string;
    // A terminal newline leaves one empty last row; it is not a body row.
    if (raw.length === 0 && index === rows.length - 1) continue;
    const line = raw.endsWith("\r") ? raw.slice(0, -1) : raw;
    const trimmed = line.trim();
    if (trimmed === "*** Begin Patch" || trimmed === "*** End Patch") continue;
    if (trimmed.length >= 2 && trimmed.startsWith("[") && trimmed.endsWith("]")) {
      const inner = trimmed.slice(1, -1);
      const hash = inner.indexOf("#");
      if (hash <= 0) return undefined;
      sections.push({ path: inner.slice(0, hash), moves: [] });
      continue;
    }
    const directive = /^(PUT|CUT|REM|MV)(?:\s|$)/.exec(line);
    if (directive) {
      const current = sections[sections.length - 1];
      if (current === undefined) return undefined;
      if (directive[1] === "MV") {
        const destination = line.slice(2).trim();
        if (destination.length === 0 || /\s/.test(destination)) return undefined;
        current.moves.push(unquote(destination));
      }
      continue;
    }
    if (trimmed.length === 0 && sections.length === 0) continue;
    if (sections.length === 0 || !line.startsWith("+")) return undefined;
  }
  return sections.length > 0 ? sections : undefined;
}

/**
 * Every file a hashline patch names: each `[path#TAG]` section header and each
 * `MV` destination, in patch order (see `hashlinePatchSections`). Undefined
 * when the patch cannot be read: the caller must then fail toward asking.
 */
export function hashlinePatchPaths(patch: string): string[] | undefined {
  return hashlinePatchSections(patch)?.flatMap((section) => [section.path, ...section.moves]);
}

const APPLY_PATCH_TARGETS = [
  "*** Add File:",
  "*** Update File:",
  "*** Delete File:",
  "*** Move to:",
];

/**
 * Every file an `apply_patch` patch names: `*** Add/Update/Delete File:` and
 * `*** Move to:` lines, wherever they appear. Undefined when it names none or
 * a target is empty, so the caller fails toward asking.
 */
export function applyPatchPaths(patch: string): string[] | undefined {
  const paths: string[] = [];
  for (const raw of patch.split("\n")) {
    const line = raw.trim();
    const prefix = APPLY_PATCH_TARGETS.find((candidate) => line.startsWith(candidate));
    if (prefix === undefined) continue;
    const path = line.slice(prefix.length).trim();
    if (path.length === 0) return undefined;
    paths.push(path);
  }
  return paths.length > 0 ? paths : undefined;
}

/**
 * The files a call's patch argument names, `[]` when it carries no patch, or
 * undefined when its targets cannot be read.
 */
function patchTargets(tool: string, args: unknown): string[] | undefined {
  if (args === null || typeof args !== "object") return [];
  const record = args as Record<string, unknown>;
  if (tool === "apply_patch") {
    const text = record.patchText ?? record.patch;
    return typeof text === "string" ? applyPatchPaths(text) : [];
  }
  return typeof record.patch === "string" ? hashlinePatchPaths(record.patch) : [];
}

/**
 * The tier OMP should see for one call of `tool`: `exec` when any path it names
 * is outside `root`, else the tool's own tier. Never throws: OMP calls this
 * synchronously inside its approval gate, where a throw would fail the call.
 */
export function ompCallTier(
  tool: string,
  args: unknown,
  root: string | undefined,
  prepare?: (args: unknown) => unknown,
): OmpToolTier {
  const natural = OMP_TOOL_TIERS[tool] ?? "exec";
  if (natural === "exec" || !root) return natural;
  try {
    const prepared = prepare ? prepare(args) : args;
    const patched = patchTargets(tool, prepared);
    // A patch the gate cannot read may name any file: fail toward asking.
    if (patched === undefined) return "exec";
    const outside = [...pathArguments(prepared), ...patched].some((path) =>
      namesOutsidePath(root, path),
    );
    return outside && TIER_RANK.exec > TIER_RANK[natural] ? "exec" : natural;
  } catch {
    // Arguments OMP will reject anyway; the tool's own tier is enough here.
    return natural;
  }
}

/**
 * The `approval` value to declare for `tool` on OMP, or undefined for a tool
 * AFT does not classify. It never carries a `policy`: an explicit `prompt`
 * would make OMP ask even in `yolo`, and only the mode should decide that.
 */
export function ompApprovalFor(
  tool: string,
  projectRoot: () => string | undefined,
  prepare?: (args: unknown) => unknown,
): ((args: unknown) => OmpToolTier) | undefined {
  if (!(tool in OMP_TOOL_TIERS)) return undefined;
  return (args: unknown) => ompCallTier(tool, args, projectRoot(), prepare);
}
