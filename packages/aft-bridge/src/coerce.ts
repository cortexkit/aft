/**
 * Coerce a tool argument that is contractually a string array into a real
 * `string[]`, tolerating the shapes models/MCP clients send in practice.
 *
 * Some hosts deliver an array-typed param as a bare string (`"a.ts"`) or a
 * JSON-stringified array (`'["a.ts","b.ts"]'`) despite the declared schema.
 * A plain `args.files as string[]` cast then lies, and the first `.map`/
 * iteration throws (`inputs.map is not a function`) before any validation can
 * report a clean error. This normalizes at the boundary so callers get a real
 * array (possibly empty) and never crash on a mistyped argument.
 *
 * Accepts:
 *  - a string[] (non-string entries dropped, empties trimmed out)
 *  - a JSON-stringified array of strings (`'["a","b"]'`)
 *  - a single non-empty string (treated as a one-element array)
 *
 * Returns `[]` for null/undefined/empty/other shapes; the caller enforces any
 * "at least one" requirement and produces the user-facing error.
 */
export function coerceStringArray(value: unknown): string[] {
  if (Array.isArray(value)) {
    return value.filter((entry): entry is string => typeof entry === "string" && entry.length > 0);
  }
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed.length === 0) return [];
    // JSON-stringified array, e.g. '["a.ts","b.ts"]'
    if (trimmed.startsWith("[") && trimmed.endsWith("]")) {
      try {
        const parsed = JSON.parse(trimmed);
        if (Array.isArray(parsed)) {
          return parsed.filter(
            (entry): entry is string => typeof entry === "string" && entry.length > 0,
          );
        }
      } catch {
        // Not valid JSON; fall through to single-string handling.
      }
    }
    // A single path. Do NOT split on spaces/commas: paths may contain spaces.
    return [value];
  }
  return [];
}

/**
 * Coerce a string param that has a compatibility alias in another harness.
 *
 * Models cross-pollinate tool schemas between hosts (e.g. sending `filePath` to
 * Pi's `read`, whose schema field is `path`). Use this at the tool boundary to
 * accept the alias only when the declared field is absent.
 *
 * Conservative by design:
 *  - if the declared field is present, its value wins unchanged — even if it is
 *    malformed or empty. We never override an explicit schema field.
 *  - the alias is accepted only when it is a non-empty string after trimming.
 *    The original alias text is returned unchanged so paths containing spaces are
 *    preserved byte-for-byte.
 */
export function coerceAliasedStringParam(value: unknown, aliasValue: unknown): string | undefined {
  if (value !== undefined) {
    return typeof value === "string" ? value : undefined;
  }
  if (typeof aliasValue !== "string") return undefined;
  return aliasValue.trim().length > 0 ? aliasValue : undefined;
}

/**
 * Coerce a polymorphic `string | string[]` tool argument (e.g. aft_outline's
 * `target`, which is a single path/URL OR an array of paths). Hosts deliver an
 * array as a JSON-stringified string (`'["a","b"]'`) despite the declared union,
 * which a naive consumer then treats as ONE literal path — aft_outline tried to
 * stat a file literally named `["src/a", "src/b"]` and failed.
 *
 * Returns:
 *  - an array if `value` is already an array (non-string/empty entries dropped),
 *    or a JSON-stringified array of strings;
 *  - the original string otherwise (single path/URL — single-target semantics
 *    are preserved, NOT split on spaces/commas).
 * Non-string, non-array input is returned as-is for the caller to reject.
 */
export function coerceTargetParam(value: unknown): string | string[] {
  if (Array.isArray(value)) {
    return value.filter((entry): entry is string => typeof entry === "string" && entry.length > 0);
  }
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed.startsWith("[") && trimmed.endsWith("]")) {
      try {
        const parsed = JSON.parse(trimmed);
        if (Array.isArray(parsed)) {
          return parsed.filter(
            (entry): entry is string => typeof entry === "string" && entry.length > 0,
          );
        }
      } catch {
        // Not valid JSON; fall through and treat as a single literal path.
      }
    }
    return value;
  }
  return value as string;
}

/**
 * Coerce a tool argument that is contractually a boolean into a real boolean,
 * tolerating the shapes models send in practice.
 *
 * Like array and integer params (see `coerceStringArray` / `coerceOptionalInt`),
 * hosts deliver a boolean-typed param as the model's raw emitted value WITHOUT
 * coercing it to the declared schema type — and models non-deterministically
 * emit `true` as the string `"true"` (or `1` / `"1"`). A strict `args.x === true`
 * check then reads a stringified `"true"` as false, silently dropping the flag.
 * This bit `aft_delete`'s `recursive`: an agent passing `recursive: true` got
 * "is a directory, pass recursive: true" because the wire value was `"true"`.
 *
 * Conservative by design: only values that UNAMBIGUOUSLY mean true coerce to
 * true (`true`, case-insensitive `"true"`, `1`, `"1"`). Everything else —
 * `false`, `"false"`, `0`, `""`, `null`, `undefined`, objects — is `false`. A
 * false-negative just re-surfaces the original "pass the flag" error (safe); a
 * false-positive on a destructive gate like `recursive` would not be, so the
 * truthy set is kept tight rather than accepting arbitrary truthy values.
 *
 * Pass `defaultValue: true` only for contractually default-true options. In
 * that mode, missing values stay enabled and only explicit false-like values
 * (`false`, `0`, `"false"`, `"0"`) disable the option.
 */
export function coerceBoolean(value: unknown, defaultValue = false): boolean {
  if (defaultValue) {
    if (value === undefined) return true;
    if (typeof value === "boolean") return value;
    if (typeof value === "number") return value !== 0;
    if (typeof value === "string") {
      const normalized = value.trim().toLowerCase();
      return normalized !== "false" && normalized !== "0";
    }
    return true;
  }
  if (typeof value === "boolean") return value;
  if (typeof value === "number") return value === 1;
  if (typeof value === "string") {
    const normalized = value.trim().toLowerCase();
    return normalized === "true" || normalized === "1";
  }
  return false;
}

/**
 * Runtime coercion for agent-friendly sentinel handling.
 *
 * Some agents emit null / "" / 0 when they mean "param not provided".
 * Use this inside tool handlers BEFORE relying on the value. Returns
 * `undefined` for all empty sentinels; rejects out-of-bounds with a
 * clear message.
 *
 * Tool handlers that want sentinel tolerance must pass args through
 * this AFTER schema validation has accepted the value (or for fields
 * declared as `unknown`/`any` that bypass type validation). Host-specific
 * optional-integer schemas stay local to each plugin; this helper is the
 * shared runtime coercion.
 */
export function coerceOptionalInt(
  v: unknown,
  paramName: string,
  min: number,
  max: number,
): number | undefined {
  if (v === undefined || v === null || v === "") return undefined;
  // 0 is an empty-param sentinel only when 0 is outside the field's accepted
  // domain. Fields whose contract permits zero must retain it for validation.
  if (typeof v === "number" && (!Number.isFinite(v) || (v === 0 && min > 0))) return undefined;
  const n = typeof v === "string" ? Number(v) : v;
  if (typeof n !== "number" || !Number.isInteger(n)) {
    throw new Error(`${paramName} must be an integer between ${min} and ${max}`);
  }
  if (n < min || n > max) {
    throw new Error(`${paramName} must be between ${min} and ${max}`);
  }
  return n;
}

/**
 * True when a value represents "agent did not provide this param".
 *
 * GPT-family models send empty strings / empty arrays / null instead of
 * omitting optional params entirely. Use this BEFORE mutual-exclusion
 * checks so an empty `targets: []` or `url: ""` doesn't get counted as
 * present and trigger a misleading "X is mutually exclusive with Y" error.
 *
 * Treats undefined / null / "" / [] / {} as empty. Booleans and numbers
 * (including 0 and false) are NOT empty by themselves — only string and
 * collection sentinels qualify.
 */
export function isEmptyParam(value: unknown): boolean {
  if (value === undefined || value === null) return true;
  if (typeof value === "string") return value.length === 0;
  if (Array.isArray(value)) return value.length === 0;
  if (typeof value === "object") return Object.keys(value as object).length === 0;
  return false;
}

/** Decode collection-shaped model arguments without treating malformed JSON as a literal value. */
export function coerceJsonCollectionParam(
  value: unknown,
  field: "batch" | "targets" | "sections",
): unknown {
  const shape =
    field !== "sections" ? "an array of {target, symbols}" : "a string or array of strings";
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed.startsWith("[") || trimmed.startsWith("{")) {
      try {
        value = JSON.parse(trimmed);
      } catch {
        throw new Error(`${field} must be ${shape} (got a string that is not valid JSON)`);
      }
    } else if (field !== "sections") {
      throw new Error(`${field} must be ${shape} (got a string that is not valid JSON)`);
    }
  }
  if (value === undefined || value === null) return value;
  if (field !== "sections") {
    if (Array.isArray(value) || typeof value === "object") return value;
  } else if (
    typeof value === "string" ||
    (Array.isArray(value) && value.every((entry) => typeof entry === "string"))
  ) {
    return value;
  }
  throw new Error(`${field} must be ${shape} (got an invalid type)`);
}

/**
 * True when a value is an empty placeholder: anything `isEmptyParam` treats
 * as empty, plus a string of only whitespace. Some models fill every declared
 * tool property on every call and send `" "` as readily as `""`.
 */
export function isBlankParam(value: unknown): boolean {
  if (typeof value === "string") return value.trim().length === 0;
  return isEmptyParam(value);
}

/**
 * The `aft_zoom` `targets` entries that carry real input, as an array.
 *
 * Accepts the decoded `targets` value (a single `{target, symbols}` object or
 * an array of them). An entry whose location and symbols are both
 * blank is a placeholder and is dropped, so `targets: {target: "", symbols: ""}`
 * or `[]` yields no entries and counts as not supplied. Every other entry,
 * including one with only a path or only a symbol, is kept; the server
 * reports such an entry as its own per-target error line.
 */
export function usableZoomTargets(targets: unknown): unknown[] {
  if (isEmptyParam(targets)) return [];
  const entries = Array.isArray(targets) ? targets : [targets];
  return entries.filter((entry) => {
    if (entry === null || entry === undefined) return false;
    if (typeof entry !== "object") return true;
    const record = entry as Record<string, unknown>;
    return !(
      isBlankParam(record.target) &&
      isBlankParam(record.url) &&
      isBlankParam(record.symbols) &&
      isBlankParam(record.path) &&
      isBlankParam(record.filePath) &&
      isBlankParam(record.symbol)
    );
  });
}

/**
 * True when an `edit` call is only `edits[]` find/replace items, the one
 * shape GitHub comment resources accept.
 *
 * Unused edit fields count as absent when they carry an empty placeholder
 * (`""` or null, and `0` for the 1-based line fields): models that fill
 * every declared property send them on every call, and refusing a valid
 * find/replace because of them would reject a correct request.
 */
export function isFindReplaceOnlyEdit(args: Record<string, unknown>): boolean {
  const absent = (value: unknown) => value === undefined || value === null || value === "";
  const absentLine = (value: unknown) => absent(value) || value === 0;
  const edits = args.edits;
  return (
    Array.isArray(edits) &&
    edits.length > 0 &&
    absent(args.appendContent) &&
    absent(args.symbol) &&
    absent(args.content) &&
    edits.every((entry) => {
      if (entry === null || typeof entry !== "object") return false;
      const item = entry as Record<string, unknown>;
      return (
        typeof item.oldString === "string" &&
        (absent(item.newString) || typeof item.newString === "string") &&
        absentLine(item.startLine) &&
        absentLine(item.endLine) &&
        absent(item.content)
      );
    })
  );
}
