/**
 * Canonical path aliases accepted at host and subc preparation boundaries.
 *
 * Compatibility is intentionally about decoded string values only. The
 * preparation layer must not trim, normalize, fold case, rewrite separators,
 * or resolve paths before comparing the two spellings.
 */

import { AftToolError } from "./error-contract.js";

export type CanonicalPathTool =
  | "read"
  | "write"
  | "edit"
  | "zoom"
  | "callgraph"
  | "safety"
  | "move"
  | "import"
  | "refactor"
  | "grep"
  | "search"
  | "conflicts";

export class InvalidRequestError extends AftToolError {
  constructor(message: string) {
    super(message, "invalid_request", {
      success: false,
      code: "invalid_request",
      message,
    });
    this.name = "InvalidRequestError";
  }
}

/** Return false for lone UTF-16 surrogate code units. */
export function isWellFormedUnicodeString(value: string): boolean {
  for (let index = 0; index < value.length; index++) {
    const codeUnit = value.charCodeAt(index);
    if (codeUnit >= 0xd800 && codeUnit <= 0xdbff) {
      const next = value.charCodeAt(index + 1);
      if (next < 0xdc00 || next > 0xdfff || Number.isNaN(next)) return false;
      index++;
    } else if (codeUnit >= 0xdc00 && codeUnit <= 0xdfff) {
      return false;
    }
  }
  return true;
}

function hasOwn(record: Record<string, unknown>, key: string): boolean {
  return Object.hasOwn(record, key);
}

function invalidPathValue(property: string): never {
  throw new InvalidRequestError(`'${property}' must be a non-empty well-formed Unicode string`);
}

function pathValue(record: Record<string, unknown>, property: string): string {
  const value = record[property];
  if (typeof value !== "string" || value.length === 0 || !isWellFormedUnicodeString(value)) {
    invalidPathValue(property);
  }
  return value;
}

function normalizeAliasPair(
  record: Record<string, unknown>,
  canonical: string,
  legacy: string,
  required: boolean,
): void {
  // An empty-string or null value on a path-shaped field means the field was
  // not supplied in substance. Strip it before presence counting so hosts that
  // serialize unused optional fields (e.g. `path: ""`) do not trip validation,
  // and a required field that is only an empty sentinel reports the
  // missing-required error rather than the well-formed-Unicode one.
  if (isNullOrEmptyString(record[canonical])) delete record[canonical];
  if (isNullOrEmptyString(record[legacy])) delete record[legacy];

  const hasCanonical = hasOwn(record, canonical);
  const hasLegacy = hasOwn(record, legacy);

  if (!hasCanonical && !hasLegacy) {
    if (required) {
      throw new InvalidRequestError(`'${canonical}' is required`);
    }
    return;
  }

  if (hasCanonical && hasLegacy) {
    let canonicalValue: string;
    let legacyValue: string;
    try {
      canonicalValue = pathValue(record, canonical);
      legacyValue = pathValue(record, legacy);
    } catch {
      throw new InvalidRequestError(
        `Invalid request: '${canonical}' and '${legacy}' must both be non-empty well-formed Unicode strings`,
      );
    }
    if (canonicalValue !== legacyValue) {
      throw new InvalidRequestError(
        `Invalid request: '${canonical}' and '${legacy}' must contain equal decoded strings`,
      );
    }
    delete record[legacy];
    return;
  }

  if (hasCanonical) {
    pathValue(record, canonical);
    return;
  }

  record[canonical] = pathValue(record, legacy);
  delete record[legacy];
}

function validateOptionalCanonicalPath(record: Record<string, unknown>, property: string): void {
  if (!hasOwn(record, property)) return;
  // An empty-string or null sentinel on an optional path field means the field
  // was not supplied; strip it so it cannot trip the well-formed check.
  if (isNullOrEmptyString(record[property])) {
    delete record[property];
    return;
  }
  pathValue(record, property);
}

function normalizeZoomTargets(record: Record<string, unknown>): void {
  if (!hasOwn(record, "targets")) return;
  const targets = record.targets;
  const normalizeTarget = (target: unknown, index: number): Record<string, unknown> => {
    if (!target || typeof target !== "object" || Array.isArray(target)) {
      throw new InvalidRequestError(`'targets[${index}].path' must be a non-empty string`);
    }
    const source = target as Record<string, unknown>;
    // Model calls sometimes serialize an omitted target as an entirely empty
    // target object. Preserve that sentinel so the tool can ignore it while
    // still rejecting any target that supplies a real symbol with an empty path.
    const emptyTarget =
      source.symbol === "" &&
      ((hasOwn(source, "path") && source.path === "") ||
        (hasOwn(source, "filePath") && source.filePath === ""));
    if (emptyTarget) return { ...source };
    const normalized = { ...source };
    try {
      normalizeAliasPair(normalized, "path", "filePath", true);
    } catch (error) {
      if (error instanceof InvalidRequestError) {
        throw new InvalidRequestError(
          error.message
            .replace("'filePath'", `'targets[${index}].filePath'`)
            .replace("'path'", `'targets[${index}].path'`),
        );
      }
      throw error;
    }
    return normalized;
  };

  if (Array.isArray(targets)) {
    if (targets.length === 0) return;
    record.targets = targets.map(normalizeTarget);
    return;
  }

  if (targets && typeof targets === "object") {
    record.targets = normalizeTarget(targets, 0);
  }
}

function bareToolName(toolName: string): CanonicalPathTool | undefined {
  const bare = toolName.startsWith("aft_") ? toolName.slice(4) : toolName;
  if (
    bare === "read" ||
    bare === "write" ||
    bare === "edit" ||
    bare === "zoom" ||
    bare === "callgraph" ||
    bare === "safety" ||
    bare === "move" ||
    bare === "import" ||
    bare === "refactor" ||
    bare === "grep" ||
    bare === "search" ||
    bare === "conflicts"
  ) {
    return bare;
  }
  return undefined;
}

/**
 * Prepare raw arguments for one registered tool before schema validation.
 *
 * The returned object is a fresh object, and nested zoom targets are copied,
 * so an alias conflict or invalid value cannot partially mutate caller state.
 */
export function prepareCanonicalPathArguments(
  toolName: string,
  rawArguments: unknown,
): Record<string, unknown> {
  if (!rawArguments || typeof rawArguments !== "object" || Array.isArray(rawArguments)) {
    throw new InvalidRequestError("tool arguments must be an object");
  }

  const tool = bareToolName(toolName);
  const record = { ...(rawArguments as Record<string, unknown>) };
  if (!tool) return record;

  switch (tool) {
    case "read":
    case "write":
    case "edit":
    case "move":
    case "import":
    case "refactor":
      normalizeAliasPair(record, "path", "filePath", true);
      break;
    case "zoom":
      normalizeAliasPair(record, "path", "filePath", false);
      normalizeZoomTargets(record);
      break;
    case "callgraph":
      normalizeAliasPair(record, "path", "filePath", true);
      normalizeAliasPair(record, "toPath", "toFile", false);
      break;
    case "safety":
      normalizeAliasPair(record, "path", "filePath", false);
      break;
    case "grep":
    case "search":
    case "conflicts":
      validateOptionalCanonicalPath(record, "path");
      break;
  }

  return record;
}

const EDIT_ROOT_COMPATIBILITY_KEYS = new Set([
  "oldString",
  "newString",
  "replaceAll",
  "occurrence",
]);

const EDIT_ROOT_CANONICAL_KEYS = new Set(["path", "appendContent", "edits", "symbol", "content"]);

const EDIT_ITEM_KEYS = new Set([
  "oldString",
  "newString",
  "replaceAll",
  "occurrence",
  "startLine",
  "endLine",
  "content",
]);

const ASCII_WHITESPACE = /^[\t\n\v\f\r ]+$/;
const ASCII_TRIM = /^[\t\n\v\f\r ]+|[\t\n\v\f\r ]+$/g;
const MAX_SAFE_INTEGER = Number.MAX_SAFE_INTEGER;

/**
 * Normalize a raw edit request before a host has parsed or stripped it.
 *
 * Edit has compatibility-only top-level fields that are intentionally absent
 * from its published schema. This function therefore owns mode selection,
 * item-family validation, and scalar coercion instead of relying on a typed
 * execute handler that may never see those fields.
 */
export function prepareCanonicalEditArguments(
  toolName: string,
  rawArguments: unknown,
): Record<string, unknown> {
  if (!rawArguments || typeof rawArguments !== "object" || Array.isArray(rawArguments)) {
    throw new InvalidRequestError("tool arguments must be an object");
  }

  const raw = rawArguments as Record<string, unknown>;
  const record = copyOwnProperties(raw);
  normalizeEditPathAlias(record);

  const suppliedLineFields = ["startLine", "endLine"].filter((key) => hasOwn(record, key));
  if (suppliedLineFields.length > 0) {
    throw new InvalidRequestError(
      `edit: top-level ${suppliedLineFields.map((key) => `'${key}'`).join(" and ")} are invalid; ` +
        "line-range fields are valid only inside 'edits[]'. " +
        "Use edits: [{ startLine, endLine, content }].",
    );
  }

  const isOpenCodeRetiredBoundary = toolName === "aft_edit";
  const retiredFields = ["file", "mode"].filter((key) => hasOwn(record, key));
  if (retiredFields.length > 0 && isOpenCodeRetiredBoundary) {
    throw new InvalidRequestError(
      "aft_edit: the retired `mode`/`file` edit form is no longer supported; use `path` with " +
        "exactly one of `appendContent`, `edits`, or `symbol` plus `content`.",
    );
  }

  const unknownRootKeys = Object.getOwnPropertyNames(record)
    .filter(
      (key) =>
        !EDIT_ROOT_CANONICAL_KEYS.has(key) &&
        !EDIT_ROOT_COMPATIBILITY_KEYS.has(key) &&
        key !== "filePath",
    )
    .sort();
  if (unknownRootKeys.length > 0) {
    throw new InvalidRequestError(formatUnknownKeys(unknownRootKeys));
  }

  validateSymbolModePair(record);
  const modes = editModesPresent(record);
  if (modes.length > 1) {
    throw new InvalidRequestError(
      `edit: conflicting modes: ${modes.join(", ")}. ${OMIT_OPTIONAL_FIELDS_STEERING}`,
    );
  }
  if (modes.length === 0) {
    throw new InvalidRequestError(
      "edit: exactly one of `appendContent`, `edits`, or `symbol` plus `content` is required. " +
        OMIT_OPTIONAL_FIELDS_STEERING,
    );
  }

  const mode = modes[0];
  if (mode === "appendContent") {
    if (typeof record.appendContent !== "string") {
      throw new InvalidRequestError("edit: 'appendContent' must be a string");
    }
  } else if (mode === "edits") {
    const parsedEdits = parseEditArray(record.edits);
    record.edits = parsedEdits.map((item, index) => normalizeEditItem(item, index));
  } else if (mode === "symbol/content") {
    if (!hasOwn(record, "symbol") || typeof record.symbol !== "string") {
      throw new InvalidRequestError("edit: 'symbol' must be a string when symbol mode is selected");
    }
    if (!hasOwn(record, "content") || typeof record.content !== "string") {
      throw new InvalidRequestError(
        "edit: incomplete symbol mode: property 'content' must be a string. " +
          "Retry with `symbol` + `content`, or use `edits[]`.",
      );
    }
  } else {
    const item: Record<string, unknown> = {};
    for (const key of EDIT_ROOT_COMPATIBILITY_KEYS) {
      if (hasOwn(record, key)) item[key] = record[key];
    }
    record.edits = [normalizeEditItem(item, 0)];
    for (const key of EDIT_ROOT_COMPATIBILITY_KEYS) delete record[key];
  }

  // Canonical-only path validation is deliberately last. Alias conflicts and
  // legacy-only aliases must be decided during preparation, but a malformed
  // canonical-only path must not hide a higher-precedence edit contract error.
  validateEditPath(record);
  return record;
}

function normalizeEditPathAlias(record: Record<string, unknown>): void {
  // An empty-string or null path means the field was not supplied in
  // substance. Strip it before alias resolution so a legacy-only empty
  // sentinel reports the missing-required error rather than the
  // well-formed-Unicode one.
  if (isNullOrEmptyString(record.path)) delete record.path;
  if (isNullOrEmptyString(record.filePath)) delete record.filePath;

  const hasCanonical = hasOwn(record, "path");
  const hasLegacy = hasOwn(record, "filePath");
  if (!hasCanonical && !hasLegacy) return;

  if (hasCanonical && hasLegacy) {
    let canonical: string;
    let legacy: string;
    try {
      canonical = pathValue(record, "path");
      legacy = pathValue(record, "filePath");
    } catch {
      throw new InvalidRequestError(
        "Invalid request: 'path' and 'filePath' must both be non-empty well-formed Unicode strings",
      );
    }
    if (canonical !== legacy) {
      throw new InvalidRequestError(
        "Invalid request: 'path' and 'filePath' must contain equal decoded strings",
      );
    }
    delete record.filePath;
    return;
  }

  if (!hasCanonical) {
    record.path = pathValue(record, "filePath");
    delete record.filePath;
  }
}

function validateEditPath(record: Record<string, unknown>): void {
  if (!hasOwn(record, "path") || isNullOrEmptyString(record.path)) {
    throw new InvalidRequestError("'path' is required");
  }
  pathValue(record, "path");
}

function formatUnknownKeys(keys: string[]): string {
  return `Unrecognized keys: ${keys.map((key) => `"${key}"`).join(", ")}`;
}

function validateSymbolModePair(record: Record<string, unknown>): void {
  const completeShapes = "Retry with `symbol` + `content`, or use `edits[]`.";
  const hasSymbol = isNonEmptyString(record.symbol);
  const hasContent = isNonEmptyString(record.content);

  if (hasSymbol) {
    if (!hasOwn(record, "content")) {
      throw new InvalidRequestError(
        `edit: incomplete symbol mode: missing property 'content'. ${completeShapes}`,
      );
    }
    if (record.content === null) {
      throw new InvalidRequestError(
        `edit: incomplete symbol mode: property 'content' is null. ${completeShapes}`,
      );
    }
    return;
  }

  if (!hasContent) return;
  if (!hasOwn(record, "symbol")) {
    throw new InvalidRequestError(
      `edit: incomplete symbol mode: missing property 'symbol'. ${completeShapes}`,
    );
  }
  if (record.symbol === null) {
    throw new InvalidRequestError(
      `edit: incomplete symbol mode: property 'symbol' is null. ${completeShapes}`,
    );
  }
  throw new InvalidRequestError(
    `edit: incomplete symbol mode: property 'symbol' must be a non-empty string. ${completeShapes}`,
  );
}

function editModesPresent(record: Record<string, unknown>): string[] {
  // Some hosts serialize every optional field with an empty sentinel. Remove
  // fields that cannot select a mode so later translation cannot revive them.
  const hasAppendContent = isNonEmptyString(record.appendContent);
  if (!hasAppendContent) delete record.appendContent;

  const hasEdits = normalizeEditArraySentinels(record);
  if (!hasEdits) delete record.edits;

  const hasSymbol = isNonEmptyString(record.symbol);
  if (!hasSymbol) {
    delete record.symbol;
    if (isNullOrEmptyString(record.content)) delete record.content;
  } else if (record.content === null) {
    delete record.content;
  }

  const hasSingleEdit = isNonEmptyString(record.oldString);
  if (!hasSingleEdit) {
    for (const key of EDIT_ROOT_COMPATIBILITY_KEYS) delete record[key];
  } else {
    for (const key of ["newString", "replaceAll", "occurrence"]) {
      if (record[key] === null) delete record[key];
    }
  }

  const modes: string[] = [];
  if (hasAppendContent) modes.push("appendContent");
  if (hasEdits) modes.push("edits");
  if (hasSymbol) modes.push("symbol/content");
  if (hasSingleEdit) modes.push("oldString/newString");
  return modes;
}

function isNonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

function isNullOrEmptyString(value: unknown): boolean {
  return value === null || value === "";
}

const OMIT_OPTIONAL_FIELDS_STEERING =
  "Omit unused optional fields entirely; do not send empty strings or empty arrays for them.";

/**
 * An edits item is a serialization sentinel when every payload field carries
 * its type-default value and the real payload lives in a sibling field. Such
 * an item carries no real edit intent and must not claim the `edits` mode.
 *
 * A pure line-range item ({startLine,endLine,content}) is never a sentinel,
 * even when `content` is "", because deleting lines is real edit intent. A
 * null `oldString` with a non-null range boundary is treated the same way. A
 * real replacement has a non-empty `oldString`, so it is never a sentinel.
 * `{oldString:"", newString:"non-empty"}` is deliberately NOT a sentinel:
 * it is kept so the batch parser reports its specific empty-match error
 * instead of silently discarding a broken but intentional edit.
 */
function isEditSentinelItem(item: unknown): boolean {
  if (!item || typeof item !== "object" || Array.isArray(item)) return false;
  const record = item as Record<string, unknown>;
  const oldStringEmpty = hasOwn(record, "oldString") && isNullOrEmptyString(record.oldString);
  if (!oldStringEmpty) return false;
  // A non-null range boundary proves that a null oldString belongs to a
  // line-range item, not an all-null serialization sentinel.
  if (
    record.oldString === null &&
    ["startLine", "endLine"].some((key) => hasOwn(record, key) && record[key] !== null)
  ) {
    return false;
  }
  // Every other payload field must also carry its omitted-value sentinel.
  // Meaningful occurrence or replaceAll values must reach item-family
  // validation instead of being silently discarded.
  return (
    (!hasOwn(record, "newString") || isNullOrEmptyString(record.newString)) &&
    (!hasOwn(record, "content") || isNullOrEmptyString(record.content)) &&
    (!hasOwn(record, "replaceAll") || record.replaceAll === null || record.replaceAll === false) &&
    isDefaultOccurrence(record.occurrence)
  );
}

function hasMeaningfulFindPayload(item: Record<string, unknown>): boolean {
  return isNonEmptyString(item.oldString);
}

function isDefaultOccurrence(value: unknown): boolean {
  return value === undefined || value === null || value === 1;
}

/**
 * Filter serialization-sentinel items out of the edits array (or its
 * stringified form) and rewrite `record.edits` to the survivors. Returns
 * whether any real edit items remain, i.e. whether the edits mode is still
 * claimed. A non-empty malformed string (or a non-array root) stays an edits
 * claim so the existing parser can report its specific validation error.
 */
function normalizeEditArraySentinels(record: Record<string, unknown>): boolean {
  const value = record.edits;
  if (Array.isArray(value)) {
    const survivors = value.filter((item) => !isEditSentinelItem(item));
    if (survivors.length === 0) return false;
    record.edits = survivors;
    return true;
  }
  if (typeof value !== "string" || value.length === 0) return false;
  try {
    const parsed: unknown = JSON.parse(value);
    if (!Array.isArray(parsed)) return true;
    const survivors = parsed.filter((item) => !isEditSentinelItem(item));
    if (survivors.length === 0) return false;
    record.edits = survivors;
    return true;
  } catch {
    // A non-empty malformed string is still an edits claim so the existing
    // parser can report its specific validation error instead of no-mode.
    return true;
  }
}

function parseEditArray(value: unknown): unknown[] {
  if (typeof value === "string") {
    let parsed: unknown;
    try {
      parsed = JSON.parse(value);
    } catch {
      throw new InvalidRequestError("edit: 'edits' must contain valid JSON representing an array");
    }
    if (!Array.isArray(parsed)) {
      throw new InvalidRequestError("edit: 'edits' JSON must have an array root");
    }
    if (parsed.length === 0) {
      throw new InvalidRequestError("edit: 'edits' array must not be empty");
    }
    return parsed;
  }
  if (!Array.isArray(value)) {
    throw new InvalidRequestError("edit: 'edits' must be a non-empty array");
  }
  if (value.length === 0) {
    throw new InvalidRequestError("edit: 'edits' array must not be empty");
  }
  return value;
}

/**
 * Normalize default fields before selecting an edit family.
 *
 * A family whose payload is only omitted-value sentinels yields to the family
 * with meaningful payload. This preserves intentional line-range deletes
 * while allowing hosts that serialize unused fields to submit find/replace
 * requests without a false mixed-mode error.
 */
function normalizeEditItemSentinels(item: Record<string, unknown>): void {
  const hadRangeFields = ["startLine", "endLine", "content"].some((key) => hasOwn(item, key));

  // Null is how some hosts serialize an omitted optional property. Remove it
  // before counting either edit family so it cannot create a false conflict.
  for (const key of [
    "oldString",
    "newString",
    "replaceAll",
    "occurrence",
    "startLine",
    "endLine",
    "content",
  ]) {
    if (item[key] === null) delete item[key];
  }

  const contentIsEmpty = item.content === "";
  if (hasMeaningfulFindPayload(item) && (!hasOwn(item, "content") || contentIsEmpty)) {
    // A meaningful match wins over blank or absent line-range payload. The
    // boundaries are serializer defaults too when no range content exists.
    for (const key of ["startLine", "endLine", "content"]) delete item[key];
    // Hosts commonly emit false and 1 alongside an omitted range. They have
    // no effect on a find/replace edit, so keep the established canonical form
    // while preserving meaningful find options.
    if (hadRangeFields) {
      if (item.replaceAll === false) delete item.replaceAll;
      if (hasOwn(item, "occurrence") && isDefaultOccurrence(item.occurrence))
        delete item.occurrence;
    }
    return;
  }

  if (!isNonEmptyString(item.content)) return;

  // A meaningful range replacement wins over default find/replace fields.
  // Non-default find fields remain so the mixed-mode validator can reject
  // genuinely ambiguous requests.
  if (item.oldString === "") delete item.oldString;
  if (item.newString === "") delete item.newString;
  if (item.replaceAll === false) delete item.replaceAll;
  if (hasOwn(item, "occurrence") && isDefaultOccurrence(item.occurrence)) delete item.occurrence;
}

function normalizeEditItem(value: unknown, index: number): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new InvalidRequestError(`edit: edits[${index}] must be an object`);
  }

  const source = value as Record<string, unknown>;
  const item = copyOwnProperties(source);
  normalizeItemAlias(item, "oldString", "oldText");
  normalizeItemAlias(item, "newString", "newText");
  normalizeEditItemSentinels(item);

  const hasFindField = ["oldString", "newString", "replaceAll", "occurrence"].some((key) =>
    hasOwn(item, key),
  );
  const hasRangeField = ["startLine", "endLine", "content"].some((key) => hasOwn(item, key));
  if (hasFindField && hasRangeField) {
    throw new InvalidRequestError(`edit: edits[${index}] mixes find/replace and line-range fields`);
  }

  if (hasFindField) {
    if (!hasOwn(item, "oldString") || typeof item.oldString !== "string") {
      throw new InvalidRequestError(`edit: edits[${index}] requires string 'oldString'`);
    }
    if (hasOwn(item, "newString") && typeof item.newString !== "string") {
      throw new InvalidRequestError(`edit: edits[${index}].newString must be a string`);
    }
    coerceEditScalars(item, index);
    validateEditItemKeys(item, index);
    return item;
  }

  if (hasRangeField) {
    for (const key of ["startLine", "endLine"]) {
      // Models routinely send stringified line numbers ("3"); coerce exact
      // integer strings before validating, matching the other edit scalars.
      const value = item[key];
      if (typeof value === "string" && /^[0-9]+$/.test(value.trim())) {
        item[key] = Number(value.trim());
      }
      if (!hasOwn(item, key) || !isPositiveSafeInteger(item[key])) {
        throw new InvalidRequestError(`edit: edits[${index}].${key} must be a positive integer`);
      }
    }
    if ((item.startLine as number) > (item.endLine as number)) {
      throw new InvalidRequestError(`edit: edits[${index}] requires startLine <= endLine`);
    }
    if (!hasOwn(item, "content") || typeof item.content !== "string") {
      throw new InvalidRequestError(`edit: edits[${index}] requires string 'content'`);
    }
    validateEditItemKeys(item, index);
    return item;
  }

  throw new InvalidRequestError(`edit: edits[${index}] must be a find/replace or line-range item`);
}

function normalizeItemAlias(
  item: Record<string, unknown>,
  canonical: string,
  legacy: string,
): void {
  if (hasOwn(item, legacy)) {
    if (!hasOwn(item, canonical)) item[canonical] = item[legacy];
    delete item[legacy];
  }
}

function validateEditItemKeys(item: Record<string, unknown>, index: number): void {
  const unknown = Object.getOwnPropertyNames(item)
    .filter((key) => !EDIT_ITEM_KEYS.has(key))
    .sort();
  if (unknown.length > 0) {
    throw new InvalidRequestError(`edit: edits[${index}] contains ${formatUnknownKeys(unknown)}`);
  }
}

function coerceEditScalars(item: Record<string, unknown>, index: number): void {
  if (hasOwn(item, "replaceAll") && hasOwn(item, "occurrence")) {
    throw new InvalidRequestError(
      `edit: edits[${index}] cannot contain both 'replaceAll' and 'occurrence'`,
    );
  }
  if (hasOwn(item, "replaceAll")) item.replaceAll = coerceEditBoolean(item.replaceAll, index);

  if (hasOwn(item, "occurrence")) {
    const occurrence = coerceEditOccurrence(item.occurrence, index);
    if (occurrence === undefined) delete item.occurrence;
    else item.occurrence = occurrence;
  }
}

function coerceEditBoolean(value: unknown, index: number): boolean {
  if (typeof value === "boolean") return value;
  if (typeof value === "number" && Number.isFinite(value) && (value === 0 || value === 1)) {
    return value === 1;
  }
  if (typeof value === "string") {
    if (value === "1") return true;
    if (value === "0") return false;
    if (/^(?:true|false)$/i.test(value)) return value.toLowerCase() === "true";
  }
  throw new InvalidRequestError(
    `edit: edits[${index}].replaceAll must be a boolean, true/false string, or 0/1`,
  );
}

function coerceEditOccurrence(value: unknown, index: number): number | undefined {
  if (value === null) return undefined;
  if (typeof value === "string") {
    const trimmed = value.replace(ASCII_TRIM, "");
    if (trimmed.length === 0 || ASCII_WHITESPACE.test(trimmed)) return undefined;
    if (!/^[+]?[0-9]+$/.test(trimmed)) {
      throw new InvalidRequestError(`edit: edits[${index}].occurrence must be a positive integer`);
    }
    try {
      const parsed = BigInt(trimmed);
      if (parsed < 1n || parsed > BigInt(MAX_SAFE_INTEGER)) throw new Error("out of range");
      return Number(parsed);
    } catch {
      throw new InvalidRequestError(`edit: edits[${index}].occurrence must be a positive integer`);
    }
  }
  if (typeof value === "number" && isPositiveSafeInteger(value)) return value;
  throw new InvalidRequestError(`edit: edits[${index}].occurrence must be a positive integer`);
}

function isPositiveSafeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 1;
}

function copyOwnProperties(source: Record<string, unknown>): Record<string, unknown> {
  const copy = Object.create(null) as Record<string, unknown>;
  for (const key of Object.getOwnPropertyNames(source)) copy[key] = source[key];
  return copy;
}
