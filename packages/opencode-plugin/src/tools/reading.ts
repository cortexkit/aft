import {
  coerceBoolean,
  coerceTargetParam,
  formatZoomText,
  REVISION_REF_TOOL_NOTE,
  revisionReadsOffered,
  revisionRefArg,
} from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition, ToolResult } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { resolveGithubConfig, toolEnabled } from "../config.js";
import { prepareToolMap } from "../normalize-schemas.js";
import type { PluginContext } from "../types.js";
import {
  callToolCall,
  coerceOptionalInt,
  isEmptyParam,
  optionalInt,
  resolvePathArg,
  type ToolRuntime,
} from "./_shared.js";
import type { V2DefinitionRuntime } from "./definitions/v2.js";
import { revisionRefArgSchema, whenGhReadEnabled } from "./hoisted.js";
import {
  assertExternalDirectoryPermission,
  assertReadPermission,
  classifyPermissionError,
  permissionDeniedResponse,
  permissionRuleDenial,
} from "./permissions.js";

const z = tool.schema;
const READ_PERMISSION_CONCURRENCY = 8;
const MAX_OUTLINE_READ_ROUNDS = 8;
const MAX_ZOOM_READ_ROUNDS = 8;
const SOURCE_MANIFEST_REQUIRED =
  "requires server content_files for read authorization. The AFT binary or daemon is older than this plugin or returned an invalid source-file manifest. Update AFT to match the plugin and restart the daemon, then retry.";

/** GitHub discussion targets are fetched by the server, not read from disk. */
function isGithubPathArg(value: unknown): boolean {
  return typeof value === "string" && (value.startsWith("issue://") || value.startsWith("pr://"));
}

function isUrl(value: string): boolean {
  return value.startsWith("http://") || value.startsWith("https://");
}

function buildZoomTitle(args: {
  batch: Array<{ target: string; symbols: string | string[] }>;
}): string {
  if (args.batch.length !== 1) return `${args.batch.length} locations`;
  const entry = args.batch[0];
  return `${entry.target}#${Array.isArray(entry.symbols) ? entry.symbols.join(", ") : entry.symbols}`;
}

interface ZoomBatchSymbolResult {
  name: string;
  success: boolean;
  content?: string;
  error?: string;
}

interface ZoomBatchResult {
  complete: boolean;
  symbols: ZoomBatchSymbolResult[];
  text: string;
}

/**
 * Tool definitions for code reading commands: outline + zoom.
 */
/**
 * The outline description's steer toward focused reading. It names
 * `aft_search`, `aft_zoom` and `aft_callgraph` only when each is registered,
 * falling back to `read` for symbol reading.
 */
function outlineFocusSteer(config: PluginContext["config"]): string {
  const zoomEnabled = toolEnabled(config, "aft_zoom");
  const reader = zoomEnabled ? "aft_zoom" : "read";
  const locate = toolEnabled(config, "aft_search") ? `aft_search + ${reader}` : reader;
  const callgraph = zoomEnabled
    ? toolEnabled(config, "aft_callgraph")
      ? " aft_zoom with `callgraph:true` gives one-level forward calls-out; use aft_callgraph only for reverse callers or multi-level traces."
      : " aft_zoom with `callgraph:true` gives one-level forward calls-out."
    : "";
  return `For understanding a specific feature, prefer ${locate} on named symbols; use aft_outline on a whole directory only for high-level structure mapping.${callgraph}`;
}

export function readingTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const zoomEnabled = toolEnabled(ctx.config, "aft_zoom");
  const ghReadEnabled = resolveGithubConfig(ctx.config).read;
  const githubOutlineDescription = whenGhReadEnabled(
    ghReadEnabled,
    "GitHub issues and pull requests can be outlined with `issue://NUMBER` or `pr://NUMBER` (including `OWNER/REPO` forms).",
  );
  const githubZoomDescription = whenGhReadEnabled(
    ghReadEnabled,
    "Batch targets may use `issue://NUMBER` or `pr://NUMBER` (including `OWNER/REPO` forms) to inspect GitHub discussion ordinals.",
  );
  const outlineDescription =
    "Structural outline of source code, documentation files, or remote URLs. For code, returns symbols (functions, classes, types) with line ranges. For Markdown and HTML, returns heading hierarchy. Use this to explore structure before reading specific sections with " +
    (zoomEnabled ? "aft_zoom" : "read") +
    ". With `files: true`, the outline is breadth-first with directory rollups; drill in by outlining a subdirectory. Rows show language, symbol count, and line count.\n\n" +
    outlineFocusSteer(ctx.config) +
    "\n\n" +
    "Pass a single `target`:\n" +
    "  • file path → outline that file (with signatures)\n" +
    "  • directory path → outline source files under it\n" +
    "  • URL (http:// or https://) → fetch and outline a remote HTML/Markdown document\n" +
    "  • array of paths → outline files and directories in one call; with files:true, every path must be a directory" +
    (githubOutlineDescription ? `\n\n${githubOutlineDescription}` : "") +
    "\n\nWhen a list is cut, the reply ends with `shown N of M <unit> (<reason>) · narrow: <knobs>`; absence of that line means the list is complete.";
  const outlineArgs = {
    target: z
      .union([z.string(), z.array(z.string())])
      .describe(
        "What to outline: a file path, directory path, URL, or array of paths. The mode is auto-detected: URLs by `http://`/`https://` prefix, directories by stat; arrays may mix files and directories.",
      ),
    files: z
      .boolean()
      .optional()
      .describe(
        "Directory-only mode: when true, target must be a directory or array of directories and the result is a breadth-first file tree with directory rollups plus language, symbol, and line counts.",
      ),
    includeTests: z
      .boolean()
      .optional()
      .describe("Directory outline only: include test files. Defaults to false; tests are hidden."),
  };
  const zoomDescription =
    'Inspect code symbols or documentation sections. Pass lookups in `batch: [{ target, symbols }]`; a single lookup is a one-entry batch. Mix files and URLs in one batch, for example `batch: [{ target: "src/app.ts", symbols: "run" }, { target: "https://example.com/docs", symbols: "Overview" }]`. Each target is auto-detected as a file path or an HTTP/HTTPS URL. `symbols` is a string or string array. Markdown and HTML lookups return the section under the heading. Top-level `contextLines` and `callgraph` apply to every entry; callgraph adds calls-out and called-by annotations. Each failed lookup reports its own error line.' +
    (githubZoomDescription ? `\n\n${githubZoomDescription}` : "");
  const zoomArgs = {
    batch: z
      .array(
        z.object({
          target: z
            .string()
            .describe(
              "File path (absolute or project-relative) or HTTP/HTTPS URL, auto-detected; issue://N or pr://N when GitHub reads are enabled",
            ),
          symbols: z
            .union([z.string(), z.array(z.string())])
            .describe(
              "Symbol name for code or heading text for Markdown/HTML; a string or array of strings",
            ),
        }),
      )
      .min(1)
      .describe("One or more lookups; entries may mix file paths and URLs"),
    contextLines: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
      "Lines of context before/after each symbol (default: 3)",
    ),
    callgraph: z
      .boolean()
      .optional()
      .describe("Include call-graph annotations for every entry (default: false)"),
  };
  // `ref` exists only while `revision_reads` is on; the getters below read the
  // live config each time the host asks, so a reload is reflected.
  const offered = () => revisionReadsOffered(ctx.config);
  const tools = prepareToolMap({
    aft_outline: {
      get description() {
        return offered()
          ? `${outlineDescription}\n\n${REVISION_REF_TOOL_NOTE} Only a single file can be outlined at a revision.`
          : outlineDescription;
      },
      get args() {
        return offered() ? { ...outlineArgs, ...revisionRefArgSchema } : outlineArgs;
      },
      execute: async (args, context): Promise<string> => {
        // Coerce at the boundary: a host may deliver the string|array `target` as
        // a JSON-stringified array, which would otherwise be treated as one
        // literal path (coerceTargetParam). And a stringified "true" must enable
        // files mode (coerceBoolean).
        const target = coerceTargetParam(args.target);
        const filesMode = coerceBoolean(args.files);
        const hasIncludeTests = !isEmptyParam(args.includeTests);
        const includeTests = coerceBoolean(args.includeTests);
        // Forward a caller's `ref` even while `revision_reads` is off and the
        // schema does not offer it: the engine then refuses it by name instead
        // of silently reading the working tree.
        const ref = revisionRefArg(args.ref);
        const rawArgs: Record<string, unknown> = {
          target,
          ...(filesMode ? { files: true } : {}),
          ...(hasIncludeTests ? { includeTests } : {}),
          ...(ref !== undefined ? { ref } : {}),
        };

        const checkedReads = new Map<string, string | undefined>();
        const deniedEntries: string[] = [];
        let directoryOutline = filesMode;
        if (Array.isArray(target)) {
          if (target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }
          const allowedTargets: string[] = [];
          for (const entry of target) {
            const resolvedTarget = await resolvePathArg(ctx, context, entry);
            const kind = await permissionKindForPath(resolvedTarget);
            directoryOutline ||= kind === "directory";
            const denial = await assertPathReadPermissions(
              ctx,
              context,
              resolvedTarget,
              ref !== undefined ? "revision" : filesMode ? "directory" : "file",
              checkedReads,
            );
            if (denial) {
              if (denial !== permissionRuleDenial("read") || target.length === 1)
                return permissionDeniedResponse(denial, context);
              deniedEntries.push(`${entry}: ${denial}`);
            } else allowedTargets.push(entry);
          }
          if (!allowedTargets.length) return deniedEntries.join("\n");
          rawArgs.target = allowedTargets;
        } else {
          if (typeof target !== "string" || target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }

          const hasUrl =
            !filesMode &&
            (target.startsWith("http://") ||
              target.startsWith("https://") ||
              target.startsWith("issue://") ||
              target.startsWith("pr://"));
          if (!hasUrl) {
            const resolvedTarget = await resolvePathArg(ctx, context, target);
            const kind = await permissionKindForPath(resolvedTarget);
            directoryOutline ||= ref === undefined && kind === "directory";
            const permissionDenied = await assertPathReadPermissions(
              ctx,
              context,
              resolvedTarget,
              ref !== undefined ? "revision" : kind,
              checkedReads,
            );
            if (permissionDenied) return permissionDeniedResponse(permissionDenied, context);
          }
        }

        const excluded = new Set<string>();
        const withDenials = (text: string): string => {
          const directoryDenials =
            (context as ToolRuntime).hostVersion === 2
              ? excluded.size
                ? [`${excluded.size} files left out by read rules.`]
                : []
              : [...excluded].map((file) => `${file}: ${checkedReads.get(file)}`);
          return [text, ...deniedEntries, ...directoryDenials].join("\n");
        };
        for (let round = 0; round < MAX_OUTLINE_READ_ROUNDS; round++) {
          const response = await callToolCall(ctx, context, "outline", {
            ...rawArgs,
            ...(excluded.size ? { _exclude_read_paths: [...excluded] } : {}),
          });
          if (response.success === false)
            throw new Error((response.message as string) || "outline failed");
          // Directory results are held until every contributing file is authorized.
          // Exclusions can reveal more rows under the output budget, so check each render.
          // File outlines are checked too: the targets were authorized under their
          // requested spelling, and the manifest names each file after resolving
          // symlinks (spelled with the configured project root when the file is
          // under it), which differs when the request goes through a symlink.
          // GitHub targets carry no manifest because they read no local file.
          const contentFiles = response.content_files;
          if (!directoryOutline && contentFiles === undefined) return withDenials(response.text);
          if (
            !Array.isArray(contentFiles) ||
            !contentFiles.every((file) => typeof file === "string" && file.length > 0)
          ) {
            throw new Error(`Outline ${SOURCE_MANIFEST_REQUIRED}`);
          }
          const distinctFiles = new Set(contentFiles as string[]);
          const unchecked = [...distinctFiles].filter((file) => !checkedReads.has(file));
          const outcomes = await checkReadPaths(context, unchecked, (file, batchContext) =>
            assertReadPermission(batchContext, file),
          );
          for (const [file, denial] of outcomes) checkedReads.set(file, denial);
          let denied = 0;
          for (const file of distinctFiles) {
            const denial = checkedReads.get(file);
            if (!denial) continue;
            if (denial !== permissionRuleDenial("read"))
              return permissionDeniedResponse(denial, context);
            if (excluded.has(file))
              throw new Error("Outline server did not exclude a read-denied file");
            excluded.add(file);
            denied++;
          }
          if (denied) {
            if (distinctFiles.size === denied)
              return permissionDeniedResponse(permissionRuleDenial("read"), context);
            continue;
          }
          return withDenials(response.text);
        }
        // A denied row can reveal another file on every render. At the limit,
        // discard the held output rather than return content that is not authorized.
        throw new Error(
          `Outline read authorization limit reached after ${MAX_OUTLINE_READ_ROUNDS} renders; narrow the target to a subdirectory.`,
        );
      },
    },

    aft_zoom: {
      get description() {
        return offered()
          ? `${zoomDescription}\n\n${REVISION_REF_TOOL_NOTE} Every file of the call is read at that revision; URLs do not have revisions.`
          : zoomDescription;
      },
      get args() {
        return offered() ? { ...zoomArgs, ...revisionRefArgSchema } : zoomArgs;
      },
      execute: async (args, context): Promise<ToolResult> => {
        if (!Array.isArray(args.batch) || args.batch.length === 0)
          throw new Error("'batch' must be a non-empty array of {target, symbols}");
        // Forward unadvertised refs too, so the engine refuses stale calls
        // instead of silently reading the working tree when revision_reads is off.
        const ref = revisionRefArg(args.ref);
        if (ref !== undefined && args.batch.some((entry) => isUrl(entry.target)))
          throw new Error("'ref' cannot be used with a URL target: a URL has no revision");
        // URLs are fetched by Rust. Authorize each distinct local file before
        // dispatch, retaining rule denials as individual batch outcomes.
        const localPaths = args.batch
          .map((entry) => entry.target)
          .filter((target) => target && !isGithubPathArg(target) && !isUrl(target));
        const resolved = await Promise.all(
          localPaths.map((target) => resolvePathArg(ctx, context, target)),
        );
        const resolvedByInput = new Map(localPaths.map((input, index) => [input, resolved[index]]));
        const deniedPaths = new Map<string, string>();
        const checkedReads = new Map<string, string | undefined>();
        const distinctFiles = [...new Set(resolved)];
        if ((context as ToolRuntime).hostVersion === 2) {
          const outcomes = await checkReadPaths(context, distinctFiles, (file, batchContext) =>
            assertPathReadPermissions(
              ctx,
              batchContext,
              file,
              ref !== undefined ? "revision" : "file",
              checkedReads,
            ),
          );
          for (const [file, denial] of outcomes) if (denial) deniedPaths.set(file, denial);
        } else {
          for (const file of distinctFiles) {
            const denial = await assertPathReadPermissions(
              ctx,
              context,
              file,
              ref !== undefined ? "revision" : "file",
              checkedReads,
            );
            if (denial) {
              if (denial !== permissionRuleDenial("read"))
                return permissionDeniedResponse(denial, context);
              deniedPaths.set(file, denial);
            }
          }
        }
        const allowed = args.batch.filter((entry) => {
          const file = resolvedByInput.get(entry.target);
          return file === undefined || !deniedPaths.has(file);
        });
        const deniedText = args.batch
          .flatMap((entry) => {
            const file = resolvedByInput.get(entry.target);
            const denial = file === undefined ? undefined : deniedPaths.get(file);
            return denial ? [`${entry.target}: ${denial}`] : [];
          })
          .join("\n");
        const rawArgs: Record<string, unknown> = { batch: allowed };
        if (ref !== undefined) rawArgs.ref = ref;
        const contextLines = coerceOptionalInt(
          args.contextLines,
          "contextLines",
          1,
          Number.MAX_SAFE_INTEGER,
        );
        if (contextLines !== undefined) rawArgs.contextLines = contextLines;
        if (coerceBoolean(args.callgraph)) rawArgs.callgraph = true;
        const title = buildZoomTitle({ batch: args.batch });
        const withMeta = (output: string): ToolResult => ({
          output,
          title,
          metadata: { title, ...rawArgs, batch: JSON.stringify(args.batch) },
        });
        if (!allowed.length) {
          const denial = deniedPaths.values().next().value;
          if (args.batch.length === 1 && denial) return permissionDeniedResponse(denial, context);
          return withMeta(deniedText);
        }
        const excluded = new Set<string>();
        for (let round = 0; round < MAX_ZOOM_READ_ROUNDS; round++) {
          const response = await callToolCall(ctx, context, "zoom", {
            ...rawArgs,
            ...(excluded.size ? { _exclude_read_paths: [...excluded] } : {}),
          });
          if (response.success === false)
            throw new Error(response.text || response.message || "zoom failed");
          // Re-exports can supply bodies or menu signatures from another file.
          // Hold all text until the server's complete source manifest is checked.
          if (
            !Array.isArray(response.content_files) ||
            !response.content_files.every((file) => typeof file === "string" && file.length > 0)
          )
            throw new Error(`Zoom ${SOURCE_MANIFEST_REQUIRED}`);
          const files = new Set(response.content_files as string[]);
          const outcomes = await checkReadPaths(
            context,
            [...files].filter((file) => !checkedReads.has(file)),
            (file, batchContext) =>
              assertPathReadPermissions(
                ctx,
                batchContext,
                file,
                ref !== undefined ? "revision" : "file",
                checkedReads,
              ),
          );
          let denied = false;
          for (const [file, denial] of outcomes) checkedReads.set(file, denial);
          for (const file of files) {
            const denial = checkedReads.get(file);
            if (!denial) continue;
            if (denial !== permissionRuleDenial("read"))
              return permissionDeniedResponse(denial, context);
            if (excluded.has(file))
              throw new Error("Zoom server did not exclude a read-denied file");
            excluded.add(file);
            denied = true;
          }
          if (!denied) return withMeta([response.text, deniedText].filter(Boolean).join("\n\n"));
        }
        throw new Error(
          `Zoom read authorization limit reached after ${MAX_ZOOM_READ_ROUNDS} renders; narrow the batch.`,
        );
      },
    },
  });
  return tools;
}

/**
 * Format multi-symbol zoom results as plain text. Successful entries use
 * `formatZoomText` (line-numbered, no JSON escapes); failures render as
 * `Symbol "name" not found: <reason>`. Sections are blank-line separated.
 *
 * Exported for regression tests.
 */
export function formatZoomBatchResult(
  targetLabel: string,
  symbols: string[],
  responses: Record<string, unknown>[],
): ZoomBatchResult {
  const entries = symbols.map((name, index): ZoomBatchSymbolResult => {
    const response = responses[index] ?? { success: false, message: "missing zoom response" };
    if (response.success === false) {
      const message =
        typeof response.message === "string" && response.message.length > 0
          ? response.message
          : "zoom failed";
      return { name, success: false, error: message };
    }
    return { name, success: true, content: formatZoomText(targetLabel, response) };
  });
  const complete = entries.every((entry) => entry.success);
  const sections: string[] = [];
  if (!complete) {
    sections.push("Incomplete zoom results: one or more symbols failed.");
  }
  for (const entry of entries) {
    if (entry.success) {
      sections.push(entry.content ?? "");
    } else {
      sections.push(`Symbol "${entry.name}" not found: ${entry.error ?? "zoom failed"}`);
    }
  }
  return { complete, symbols: entries, text: sections.join("\n\n") };
}

/** Bound host traffic without letting completion order change per-file outcomes. */
async function checkReadPaths(
  context: ToolContext,
  files: string[],
  check: (file: string, context: ToolContext) => Promise<string | undefined>,
): Promise<Map<string, string | undefined>> {
  // OC1 ask accepts one request and returns a Promise, with no cancellation
  // channel for concurrent asks. Keep OC1 asks sequential instead of using OC2's adapter.
  if ((context as ToolRuntime).hostVersion !== 2) {
    const results = new Map<string, string | undefined>();
    for (const file of files) {
      context.abort.throwIfAborted();
      const denial = await check(file, context);
      if (denial && denial !== permissionRuleDenial("read")) throw new Error(denial);
      results.set(file, denial);
    }
    return results;
  }
  const controller = new AbortController();
  const ask = context.ask as V2DefinitionRuntime["ask"];
  const batchContext: ToolContext = {
    ...context,
    ask: (request) => {
      context.abort.throwIfAborted();
      controller.signal.throwIfAborted();
      return ask(request, controller.signal).catch((error) => {
        if (classifyPermissionError(error).kind !== "rule_denied") controller.abort(error);
        throw error;
      });
    },
  };
  const results: Array<string | undefined> = [];
  let next = 0;
  const worker = async () => {
    try {
      for (;;) {
        context.abort.throwIfAborted();
        controller.signal.throwIfAborted();
        const index = next++;
        const file = files[index];
        if (file === undefined) return;
        results[index] = await check(file, batchContext);
        controller.signal.throwIfAborted();
      }
    } catch (error) {
      controller.abort(error);
      throw error;
    }
  };
  const workers = Array.from(
    { length: Math.min(READ_PERMISSION_CONCURRENCY, files.length) },
    worker,
  );
  try {
    await Promise.all(workers);
  } catch {
    // Rule denials remain per-file outcomes. A user rejection instead stops
    // new asks and interrupts siblings before this tool reports one failure.
    await Promise.allSettled(workers);
    throw controller.signal.reason;
  }
  return new Map(files.map((file, index) => [file, results[index]]));
}

async function permissionKindForPath(resolvedPath: string): Promise<"file" | "directory"> {
  try {
    const { stat } = await import("node:fs/promises");
    const st = await stat(resolvedPath);
    return st.isDirectory() ? "directory" : "file";
  } catch {
    // If stat fails, keep the tool call moving so the server can report the
    // real path error. Use a file label because a missing path cannot be a
    // directory that the permission prompt could grant.
    return "file";
  }
}

async function assertPathReadPermissions(
  ctx: PluginContext,
  context: ToolContext,
  target: string | string[],
  kind: "file" | "directory" | "revision" = "file",
  checkedReads = new Map<string, string | undefined>(),
): Promise<string | undefined> {
  const targets = Array.isArray(target) ? target : [target];
  const checked = new Set<string>();

  for (const resolvedPath of targets) {
    if (typeof resolvedPath !== "string" || resolvedPath.length === 0) continue;
    const key = `${kind}:${resolvedPath}`;
    if (checked.has(key)) continue;
    checked.add(key);

    const denial = await assertExternalDirectoryPermission(ctx, context, resolvedPath, {
      kind: kind === "directory" ? "directory" : "file",
      serverValidatedRead: true,
    });
    if (denial) return denial;
    // A revision file need not be a file in the working tree anymore.
    if (
      kind === "revision" ||
      (kind === "file" && (await permissionKindForPath(resolvedPath)) === "file")
    ) {
      if (!checkedReads.has(resolvedPath))
        checkedReads.set(resolvedPath, await assertReadPermission(context, resolvedPath));
      const readDenial = checkedReads.get(resolvedPath);
      if (readDenial) return readDenial;
    }
  }

  return undefined;
}
