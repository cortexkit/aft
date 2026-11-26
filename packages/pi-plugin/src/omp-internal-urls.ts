import { coerceOptionalInt } from "@cortexkit/aft-bridge";
import type {
  AgentToolResult,
  ExtensionContext,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { Text } from "@earendil-works/pi-tui/dist/components/text.js";
import { type Static, type TSchema, Type } from "typebox";
import { Value } from "typebox/value";
import { detectPiHarness } from "./harness.js";
import { warn } from "./logger.js";
import {
  asRecord,
  asString,
  collapsibleResult,
  type RenderContextLike,
  type RenderResultOptionsLike,
} from "./tools/render-helpers.js";

/** The host-owned router API used by the overrides; no URL handlers live in AFT. */
export interface OmpInternalUrlRouter {
  canHandle(input: string): boolean;
  canResolve(input: string): boolean;
  writeTarget(input: string): { spec: { write?: unknown } } | undefined;
}

let routerWarningEmitted = false;

export async function loadOmpInternalUrlRouter(
  api: unknown,
): Promise<OmpInternalUrlRouter | undefined> {
  if (detectPiHarness(api) !== "omp") return undefined;
  try {
    // Keep the literal import: OMP's extension loader resolves host subpaths,
    // including in standalone binaries where there is no host node_modules.
    const { InternalUrlRouter } = await import("@oh-my-pi/pi-coding-agent/internal-urls");
    const router = InternalUrlRouter.instance();
    if (
      typeof router.canHandle === "function" &&
      typeof router.canResolve === "function" &&
      typeof router.writeTarget === "function"
    )
      return router;
    throw new Error("OMP's internal URL router lacks canHandle/canResolve/writeTarget");
  } catch (error) {
    if (!routerWarningEmitted) {
      routerWarningEmitted = true;
      warn(
        `OMP internal URL delegation is unavailable: ${error instanceof Error ? error.message : String(error)}. AFT will use its filesystem tools; check the OMP host's internal-urls export.`,
      );
    }
  }
  return undefined;
}

type OmpContext = ExtensionContext & {
  invokeTool?: (
    params: Record<string, unknown>,
    options?: { signal?: AbortSignal; onUpdate?: (result: AgentToolResult<unknown>) => void },
  ) => Promise<AgentToolResult<unknown>>;
};

function pathEntries(value: unknown): string[] {
  if (Array.isArray(value)) return value.flatMap(pathEntries);
  if (typeof value !== "string") return [];
  // Test the unsplit path first: commas and semicolons can be literal URL data.
  return [value, ...value.split(/[,;\n]/).map((path) => path.trim())];
}

/** Native bash owns the URL filesystem, even for URLs inside quoted words. */
export function bashHasInternalUrl(command: unknown, router: OmpInternalUrlRouter): boolean {
  if (typeof command !== "string") return false;
  // This is a conservative routing scan, not shell evaluation. Splitting quoted
  // text may over-delegate echo/heredoc content; native bash still executes it.
  return command.split(/[\s|&;()<>]+/).some((word) => {
    const stripQuotes = (value: string) => value.replace(/^["'`]+|["'`]+$/g, "");
    if (router.canHandle(stripQuotes(word))) return true;
    // URI arguments can also be shell assignments or --option=URI values.
    const equals = word.indexOf("=");
    return equals !== -1 && router.canHandle(stripQuotes(word.slice(equals + 1)));
  });
}

function internalTarget(
  name: string,
  params: unknown,
  router: OmpInternalUrlRouter,
): string | undefined {
  const args = asRecord(params) ?? {};
  if (name === "bash") {
    for (const cwd of [args.cwd, args.workdir]) {
      if (typeof cwd === "string" && router.canHandle(cwd)) return cwd;
    }
    return bashHasInternalUrl(args.command, router) ? asString(args.command) : undefined;
  }
  const paths = name === "grep" ? [args.path, args.paths] : [args.path];
  for (const path of paths.flatMap(pathEntries)) {
    if (name === "read" || name === "grep") {
      if (router.canResolve(path)) return path;
    } else if (router.canHandle(path) && router.writeTarget(path)?.spec.write) {
      // Read-only schemes (notably issue/pr) retain AFT's own write semantics.
      // Handler-owned edit refusals remain the native edit tool's responsibility.
      return path;
    }
  }
  return undefined;
}

/** Accept native inputs before execute, without changing upstream Pi schemas. */
function schemaDocument(schema: TSchema): TSchema {
  // OMP's TypeBox compatibility layer returns callable omptype schemas, not
  // plain TypeBox objects. Their public emitter preserves required/optional
  // fields; inspecting .properties on the callable would see an empty schema.
  const emitter = schema as unknown as { toJsonSchema?: () => TSchema };
  return typeof emitter.toJsonSchema === "function" ? emitter.toJsonSchema() : schema;
}

function objectProperties(schema: TSchema): Record<string, TSchema> {
  const document = asRecord(schema) ?? {};
  const required = new Set(Array.isArray(document.required) ? document.required : []);
  const properties: Record<string, TSchema> = {};
  for (const [key, value] of Object.entries(asRecord(document.properties) ?? {})) {
    const property = Type.Unsafe(value as TSchema);
    properties[key] = required.has(key) ? property : Type.Optional(property);
  }
  return properties;
}

function ompParameters(name: string, original: TSchema): TSchema {
  const originalProperties = asRecord(asRecord(original)?.properties) ?? {};
  const properties = objectProperties(original);
  if (name === "read") {
    // Native read only declares path; it ignores AFT's paging fields. Let URL
    // calls reach it even with values AFT would reject for a filesystem read.
    for (const key of ["startLine", "endLine", "offset", "limit"]) {
      properties[key] = Type.Optional(
        Type.Unknown({ description: asString(asRecord(originalProperties[key])?.description) }),
      );
    }
  } else if (name === "write") {
    properties.content = Type.Optional(
      Type.String({ description: "Full contents; may be omitted for proc://<id>/kill." }),
    );
  } else if (name === "edit") {
    // OMP offers replace, patch and input-only edit modes. AFT may itself be in
    // hashline mode (patch required, additionalProperties:false); neither schema
    // may reject a native URL edit before the host can handle it.
    for (const key of Object.keys(properties)) properties[key] = Type.Optional(properties[key]);
    properties.path = Type.Optional(Type.String());
    properties.input = Type.Optional(Type.String());
    properties.old_string = Type.Optional(Type.String());
    properties.new_string = Type.Optional(Type.String());
    properties.replace_all = Type.Optional(Type.Boolean());
    const aftEntry = asRecord(originalProperties.edits)?.items;
    properties.edits = Type.Optional(
      Type.Array(
        Type.Object({
          ...objectProperties((aftEntry ?? {}) as TSchema),
          op: Type.Optional(
            Type.Union([Type.Literal("create"), Type.Literal("delete"), Type.Literal("update")]),
          ),
          rename: Type.Optional(Type.String()),
          diff: Type.Optional(Type.String()),
        }),
      ),
    );
  } else if (name === "grep") {
    properties.path = Type.Optional(Type.Union([Type.String(), Type.Array(Type.String())]));
    properties.paths = Type.Optional(Type.Array(Type.String()));
    properties.case = Type.Optional(Type.Boolean());
    properties.gitignore = Type.Optional(Type.Boolean());
    properties.skip = Type.Optional(Type.Union([Type.Number(), Type.Null()]));
  } else if (name === "bash") {
    properties.timeout = Type.Optional(Type.Union([Type.Number(), Type.String()]));
    properties.cwd = Type.Optional(Type.String());
    properties.pty = Type.Optional(Type.Boolean());
    properties.async = Type.Optional(Type.Boolean());
    properties.name = Type.Optional(Type.String({ maxLength: 48 }));
    properties.ready = Type.Optional(
      Type.Object({
        log: Type.Optional(Type.String()),
        port: Type.Optional(Type.Number()),
        host: Type.Optional(Type.String()),
        timeout: Type.Optional(Type.Number()),
      }),
    );
  }
  return Type.Object(properties);
}

function resultText(result: unknown): string {
  const content = asRecord(result)?.content;
  return Array.isArray(content)
    ? content
        .map((block) => asString(asRecord(block)?.text))
        .filter(Boolean)
        .join("\n")
    : "";
}

function filesystemForm(name: string, schema: TSchema): string {
  switch (name) {
    case "write":
      return "content is required on a file path; supply the full file contents. Omitting content is only for OMP internal URLs such as proc://<id>/kill.";
    case "edit":
      return asRecord(asRecord(schema)?.properties)?.patch
        ? "edit on a file path takes a non-empty patch string with [path#TAG] headers. Native edit arguments are only for OMP internal URLs handled by the native tool."
        : "edit on a file path takes edits[] (oldString/newString), appendContent, or symbol plus content. Native edit arguments are only for OMP internal URLs handled by the native tool.";
    case "grep":
      return "grep on file paths takes pattern, a string path, and optional include/offset. Use offset for paging instead of skip; case/gitignore/skip/paths and path arrays are only for OMP internal URLs handled by the native tool.";
    case "bash":
      return 'bash filesystem calls take command, workdir, background, and timeout in integer milliseconds (numeric strings like "500" also work). cwd/async/name/ready are only for OMP internal URLs handled by native bash; use workdir/background instead of cwd/async.';
    default:
      return "read on a file path takes path and optional startLine/endLine/offset/limit as positive integers.";
  }
}

function filesystemArgumentError(name: string, schema: TSchema, fields: string[]): Error {
  return new Error(
    `${name}: invalid AFT filesystem arguments: ${fields.join(", ")}. ${filesystemForm(name, schema)}`,
  );
}

function rejectNativeOnlyArguments(name: string, schema: TSchema, params: unknown): void {
  const args = asRecord(params) ?? {};
  const nativeKeys =
    name === "edit"
      ? ["old_string", "new_string", "replace_all", "input"]
      : name === "grep"
        ? ["case", "gitignore", "skip", "paths"]
        : name === "bash"
          ? ["cwd", "async", "name", "ready"]
          : [];
  const fields = nativeKeys.filter((key) => args[key] !== undefined);
  if (name === "grep" && Array.isArray(args.path)) fields.push("path");
  if (name === "edit" && Array.isArray(args.edits)) {
    args.edits.forEach((entry, index) => {
      for (const key of ["op", "rename", "diff"]) {
        if (asRecord(entry)?.[key] !== undefined) fields.push(`edits[${index}].${key}`);
      }
    });
  }
  if (name === "bash" && args.timeout !== undefined) {
    try {
      coerceOptionalInt(args.timeout, "timeout", 1, Number.MAX_SAFE_INTEGER);
    } catch {
      fields.push("timeout");
    }
  }
  if (fields.length > 0) throw filesystemArgumentError(name, schema, fields);
}

function validateFilesystemArguments(name: string, schema: TSchema, params: unknown): void {
  if (Value.Check(schema, params)) return;
  const args = asRecord(params) ?? {};
  const object = asRecord(schema) ?? {};
  const properties = asRecord(object.properties) ?? {};
  const required = Array.isArray(object.required) ? object.required : [];
  const fields = new Set<string>();
  for (const key of required)
    if (typeof key === "string" && args[key] === undefined) fields.add(key);
  for (const [key, value] of Object.entries(args)) {
    if (properties[key] && !Value.Check(properties[key] as TSchema, value)) fields.add(key);
    else if (!properties[key] && object.additionalProperties === false) fields.add(key);
  }
  throw filesystemArgumentError(name, schema, fields.size > 0 ? [...fields] : ["arguments"]);
}

function delegatedResult(
  result: AgentToolResult<unknown>,
  target: string,
): AgentToolResult<unknown> {
  const details = asRecord(result.details);
  // Persist attribution across transcript serialization without changing any
  // object owned by the host. Non-record details remain available as nativeDetails.
  return {
    ...result,
    details: {
      ...(details ?? (result.details !== undefined ? { nativeDetails: result.details } : {})),
      delegatedTo: "omp",
      delegatedTarget: target,
    },
  };
}

export function withOmpInternalUrls<TParams extends TSchema, TDetails, TState>(
  tool: ToolDefinition<TParams, TDetails, TState>,
  router: OmpInternalUrlRouter | undefined,
): ToolDefinition<TParams, TDetails, TState> {
  if (!router || !["read", "write", "edit", "grep", "bash"].includes(tool.name)) return tool;
  const filesystemSchema = schemaDocument(tool.parameters);
  return {
    ...tool,
    ...(tool.name === "bash" ? { readsSkillUris: true } : {}),
    parameters: ompParameters(tool.name, filesystemSchema) as TParams,
    prepareArguments(args) {
      // Native URL inputs must not undergo AFT's path/edit alias normalization.
      if (internalTarget(tool.name, args, router)) return args as Static<TParams>;
      rejectNativeOnlyArguments(tool.name, filesystemSchema, args);
      return (tool.prepareArguments ? tool.prepareArguments(args) : args) as Static<TParams>;
    },
    async execute(id, params, signal, onUpdate, context) {
      const native = context as OmpContext;
      const target = internalTarget(tool.name, params, router);
      if (typeof native.invokeTool === "function" && target) {
        const result = await native.invokeTool(params as Record<string, unknown>, {
          signal,
          ...(onUpdate
            ? {
                onUpdate: (update: AgentToolResult<unknown>) => {
                  onUpdate(delegatedResult(update, target) as AgentToolResult<TDetails>);
                },
              }
            : {}),
        });
        return delegatedResult(result, target) as AgentToolResult<TDetails>;
      }
      rejectNativeOnlyArguments(tool.name, filesystemSchema, params);
      const prepared = tool.prepareArguments ? tool.prepareArguments(params) : params;
      // The OMP schema is intentionally wider. Ordinary filesystem calls still
      // obey AFT's original schema and runtime validation (e.g. required content).
      validateFilesystemArguments(tool.name, filesystemSchema, prepared);
      return tool.execute(id, prepared, signal, onUpdate, context);
    },
    renderResult(result, options = { expanded: false, isPartial: false }, theme, context) {
      const renderContext = context as RenderContextLike;
      const details = asRecord(result.details);
      if (details?.delegatedTo === "omp") {
        const target = asString(details.delegatedTarget);
        const isError = renderContext.isError || asRecord(result)?.isError === true;
        const status = isError ? "error" : options.isPartial ? "running" : "completed";
        const summary = `${tool.name} ${target ?? ""} — OMP ${status}`;
        return collapsibleResult({
          summary: theme.fg(isError ? "error" : "muted", summary),
          full: new Text(`${summary}\n${resultText(result)}`, 0, 0),
          expanded: (options as RenderResultOptionsLike).expanded,
          context: renderContext,
        });
      }
      return tool.renderResult
        ? tool.renderResult(result, options, theme, context)
        : new Text(resultText(result), 0, 0);
    },
  };
}
