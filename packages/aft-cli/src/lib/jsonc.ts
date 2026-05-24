import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { parse as parseJsonc, stringify as stringifyJsonc } from "comment-json";

export type JsoncFormat = "json" | "jsonc" | "none";

export interface JsoncFile {
  path: string;
  format: JsoncFormat;
}

/** Detect an existing {name}.jsonc or {name}.json next to a base directory. */
export function detectJsoncFile(configDir: string, baseName: string): JsoncFile {
  const jsoncPath = `${configDir}/${baseName}.jsonc`;
  const jsonPath = `${configDir}/${baseName}.json`;

  if (existsSync(jsoncPath)) {
    return { path: jsoncPath, format: "jsonc" };
  }
  if (existsSync(jsonPath)) {
    return { path: jsonPath, format: "json" };
  }
  return { path: jsonPath, format: "none" };
}

/** Parse a JSONC file; returns null on missing file or unreadable content. */
export function readJsoncFile(path: string): {
  value: Record<string, unknown> | null;
  error?: string;
} {
  if (!existsSync(path)) {
    return { value: null };
  }
  try {
    const raw = readFileSync(path, "utf-8");
    const value = parseJsonc(raw) as Record<string, unknown>;
    return { value };
  } catch (error) {
    return {
      value: null,
      error: error instanceof Error ? error.message : String(error),
    };
  }
}

/**
 * Write a JSON/JSONC file, preserving comments when possible. Creates parent
 * directories. When `format === "jsonc"` and the value was produced by
 * `comment-json`'s `parse`, embedded comments are retained via `stringifyJsonc`.
 */
export function writeJsoncFile(
  path: string,
  value: Record<string, unknown>,
  format: JsoncFormat = "json",
): void {
  mkdirSync(dirname(path), { recursive: true });
  const serialized =
    format === "jsonc" ? stringifyJsonc(value, null, 2) : JSON.stringify(value, null, 2);
  writeFileSync(path, `${serialized}\n`);
}

/** Canonical URL of the published AFT config schema. */
export const AFT_SCHEMA_URL =
  "https://raw.githubusercontent.com/cortexkit/aft/master/assets/aft.schema.json";

export type AftSchemaAction = "added" | "updated" | "unchanged";

/**
 * Ensure `aft.jsonc` (or `aft.json`) contains a top-level `$schema` field
 * pointing at the published AFT JSON Schema. Editor tooling (VS Code, Cursor,
 * etc.) uses this for autocomplete and validation.
 *
 * Creates the file with `{"$schema": "..."}` if missing. Preserves existing
 * comments and field ordering when the file exists.
 *
 * `format` follows the harness adapter contract: `"none"` means no file
 * existed before — written as `.json` by default so editors that don't grok
 * JSONC still parse it. Pass `"jsonc"` to keep existing JSONC files as JSONC.
 */
export function ensureAftSchemaUrl(
  path: string,
  format: JsoncFormat,
): { action: AftSchemaAction; message: string } {
  const existed = existsSync(path);
  if (!existed) {
    const writeFormat: JsoncFormat = format === "jsonc" ? "jsonc" : "json";
    writeJsoncFile(path, { $schema: AFT_SCHEMA_URL }, writeFormat);
    return {
      action: "added",
      message: `created ${path} with $schema URL for editor autocomplete`,
    };
  }

  const { value, error } = readJsoncFile(path);
  if (!value) {
    throw new Error(error ? `failed to parse ${path}: ${error}` : `failed to parse ${path}`);
  }

  const previous = value.$schema;
  if (previous === AFT_SCHEMA_URL) {
    return { action: "unchanged", message: `$schema already present in ${path}` };
  }

  // Mutate in place so comment-json preserves the comment associations on
  // the existing object. Spreading into a fresh literal would drop them.
  value.$schema = AFT_SCHEMA_URL;
  writeJsoncFile(path, value, format === "none" ? "json" : format);

  if (previous === undefined) {
    return {
      action: "added",
      message: `added $schema URL to ${path} for editor autocomplete`,
    };
  }
  return {
    action: "updated",
    message: `updated $schema URL in ${path}`,
  };
}
