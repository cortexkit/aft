import { realpathSync } from "node:fs";
import { homedir, userInfo } from "node:os";

export function escapeRegex(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function safeRealpath(p: string): string | null {
  try {
    return realpathSync(p);
  } catch {
    return null;
  }
}

const SECRET_PLACEHOLDER = "<REDACTED_SECRET>";
const URL_CREDENTIAL_PLACEHOLDER = "***";
const KEY_NAME = "[A-Za-z_][A-Za-z0-9_.-]*";
const SENSITIVE_KEY_WORD = /(?:token|password|secret|api[_-]?key|passwd|pwd|credential)/i;
const SEGMENTED_KEY_WORD = /(?:^|[_.-])key(?:$|[_.-])/i;
const CAMEL_CASE_KEY_WORD = /[a-z0-9]Key(?:$|[A-Z_.-])/;
const SHORT_PASSWORD_NAME = /(?:^|_)(?:pw|pass)$/i;
const JWT_PATTERN = /\beyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/g;
const AWS_ACCESS_KEY_ID_PATTERN = /\b(?:AKIA|ASIA|AGPA|AIDA|AROA)[A-Z0-9]{16}\b/g;
// Credential query parameters inside a URL. The generic key/value passes below
// cannot see these: they match the URL scheme (`https:`) as the key and swallow
// the rest of the URL as its value. Kept identical to the Rust log redactor
// (crates/aft/src/log_redact.rs); both suites read
// crates/aft/tests/fixtures/log_redaction_cases.json.
const CREDENTIAL_QUERY_PATTERN =
  /([?&](?:[a-z0-9_-]*token|api[_-]?key|apikey|[a-z0-9_-]*secret|password|passwd|pwd)=)[^&\s#"'<>]+/gi;
// `key=` is too generic to mask on its name alone; see looksLikeSecret.
const KEY_QUERY_PATTERN = /([?&]key=)([^&\s#"'<>]+)/gi;

/**
 * A `key=` query value counts as a secret when it is long, uses only
 * token-safe characters, and mixes digits with upper- and lower-case letters
 * (an API key such as `AIzaSy…`). Words, paths and single-case hex hashes do
 * not qualify.
 */
function looksLikeSecret(value: string): boolean {
  return (
    value.length >= 16 &&
    /^[A-Za-z0-9_\-.~+/=]+$/.test(value) &&
    /[0-9]/.test(value) &&
    /[A-Z]/.test(value) &&
    /[a-z]/.test(value)
  );
}

// Keep assignment paths and URLs readable, but allow base64 slashes inside
// tokens. Identical to looks_like_assignment_secret in the Rust log redactor.
function looksLikeAssignmentSecret(value: string): boolean {
  return (
    !["/", "~/", "./", "../"].some((prefix) => value.startsWith(prefix)) &&
    !value.includes("://") &&
    looksLikeSecret(value)
  );
}

const quotedSensitiveKeyValuePattern = new RegExp(
  String.raw`((['"])(${KEY_NAME})\2[^\S\r\n]*:[^\S\r\n]*)(['"])([^'"\r\n]+)\4`,
  "gi",
);
const unquotedSensitiveKeyValuePattern = new RegExp(
  String.raw`\b((${KEY_NAME})[^\S\r\n]*[=:][^\S\r\n]*)(['"])([^'"\r\n]+)\3`,
  "gi",
);
const bareSensitiveKeyValuePattern = new RegExp(
  String.raw`\b((${KEY_NAME})[^\S\r\n]*[=:][^\S\r\n]*)([^\s,;&|<>()'"]+)`,
  "gi",
);

function isSensitiveKeyName(keyName: string): boolean {
  return (
    keyName !== "PWD" &&
    (SENSITIVE_KEY_WORD.test(keyName) ||
      SEGMENTED_KEY_WORD.test(keyName) ||
      CAMEL_CASE_KEY_WORD.test(keyName) ||
      SHORT_PASSWORD_NAME.test(keyName))
  );
}

function shouldRedactAssignment(
  prefix: string,
  keyName: string,
  value: string,
  offset: number,
  content: string,
  commandContext: boolean,
): boolean {
  // The query-specific pass already decided whether a generic key is secret.
  if (keyName.toLowerCase() === "key" && /[?&]/.test(content[offset - 1] ?? "")) {
    return false;
  }
  return (
    isSensitiveKeyName(keyName) ||
    (commandContext && prefix.trimEnd().endsWith("=") && looksLikeAssignmentSecret(value))
  );
}

function redactAssignments(content: string, commandContext: boolean): string {
  let sanitized = content.replace(
    unquotedSensitiveKeyValuePattern,
    (
      match: string,
      prefix: string,
      keyName: string,
      valueQuote: string,
      value: string,
      offset: number,
      input: string,
    ) =>
      shouldRedactAssignment(prefix, keyName, value, offset, input, commandContext)
        ? `${prefix}${valueQuote}${SECRET_PLACEHOLDER}${valueQuote}`
        : match,
  );
  sanitized = sanitized.replace(
    bareSensitiveKeyValuePattern,
    (
      match: string,
      prefix: string,
      keyName: string,
      value: string,
      offset: number,
      input: string,
    ) =>
      shouldRedactAssignment(prefix, keyName, value, offset, input, commandContext)
        ? `${prefix}${SECRET_PLACEHOLDER}`
        : match,
  );
  return sanitized;
}

/**
 * Mask credentials only (tokens, URL userinfo, credential key/values, email
 * addresses), leaving paths alone. `sanitizeContent` adds the path and
 * username passes on top.
 */
export function redactSecrets(content: string): string {
  let sanitized = content;

  sanitized = sanitized.replace(
    /\b((?:Proxy-)?Authorization[^\S\r\n]*:[^\S\r\n]*(?:Bearer|Basic)[^\S\r\n]+)[A-Za-z0-9._~+/-]+=*/gi,
    `$1${SECRET_PLACEHOLDER}`,
  );
  sanitized = sanitized.replace(/\bgithub_pat_[A-Za-z0-9_]+\b/g, SECRET_PLACEHOLDER);
  sanitized = sanitized.replace(/\bgh[pousr]_[A-Za-z0-9_]{16,}\b/g, SECRET_PLACEHOLDER);
  sanitized = sanitized.replace(
    /\bsk-(?:live-)?[A-Za-z0-9][A-Za-z0-9_-]{7,}\b/g,
    SECRET_PLACEHOLDER,
  );
  sanitized = sanitized.replace(JWT_PATTERN, SECRET_PLACEHOLDER);
  sanitized = sanitized.replace(AWS_ACCESS_KEY_ID_PATTERN, SECRET_PLACEHOLDER);
  sanitized = sanitized.replace(CREDENTIAL_QUERY_PATTERN, `$1${SECRET_PLACEHOLDER}`);
  sanitized = sanitized.replace(
    KEY_QUERY_PATTERN,
    (match: string, prefix: string, value: string) =>
      looksLikeSecret(value) ? `${prefix}${SECRET_PLACEHOLDER}` : match,
  );
  sanitized = sanitized.replace(
    quotedSensitiveKeyValuePattern,
    (match: string, prefix: string, _keyQuote: string, keyName: string, valueQuote: string) =>
      isSensitiveKeyName(keyName)
        ? `${prefix}${valueQuote}${SECRET_PLACEHOLDER}${valueQuote}`
        : match,
  );
  sanitized = redactAssignments(sanitized, false);
  sanitized = sanitized.replace(
    /\b([a-z][a-z0-9+.-]*:\/\/)[^@\s/?#]+@/gi,
    `$1${URL_CREDENTIAL_PLACEHOLDER}@`,
  );
  sanitized = sanitized.replace(/\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi, "<EMAIL>");

  // Session and request IDs often look like tokens. Only the shell command
  // after cmd= is eligible for value-shape masking, not the log metadata.
  sanitized = sanitized.replace(
    /^([^\r\n]*\bbash spawned task=[^\r\n]*? cmd=)([^\r\n]*)/gm,
    (_match: string, prefix: string, command: string) =>
      `${prefix}${redactAssignments(command, true)}`,
  );

  return sanitized;
}

/**
 * Redact known project/home paths and explicit username positions in logs,
 * diagnostic JSON blocks and the final issue body, preserving ordinary prose.
 */
export function sanitizeContent(content: string, context: SanitizeContext = {}): string {
  const username = context.username ?? userInfo().username;
  const home = context.home ?? homedir();
  let sanitized = redactSecrets(content);
  const cwd = process.cwd();
  const roots = new Set([
    cwd,
    safeRealpath(cwd),
    ...(context.projectRoots ?? []).flatMap((root) =>
      root.startsWith("~/") ? [root, home + root.slice(1)] : [root],
    ),
    ...reportProjectRoots(decodedView(content).text),
  ]);
  const candidates = [...roots].filter((root): root is string => Boolean(root));
  for (const root of candidates) {
    const real = safeRealpath(root);
    if (real) roots.add(real);
  }
  // A report may include several project roots, not just the CLI's cwd.
  // Replace nested roots first so their private suffixes cannot survive.
  for (const root of [...roots]
    .filter((root): root is string => Boolean(root))
    .sort((a, b) => b.length - a.length)) {
    if (
      /^(?:\/|[A-Za-z]:[/\\]|~\/)/.test(root) &&
      root !== home &&
      !/^(?:[/\\]|[A-Za-z]:[/\\])$/.test(root)
    ) {
      sanitized = replacePath(sanitized, root, "<PROJECT>");
    }
  }
  if (home && home !== "/") sanitized = replacePath(sanitized, home, "~");
  // Only profile-directory components identify users. A common username such
  // as dev must never change /dev/null, relative paths, or ordinary prose.
  sanitized = replaceDecoded(
    sanitized,
    /(?:(?<![\p{L}\p{N}_/.-])|(?<=file:\/\/)|(?<=file:\/\/\/))(?:\/(?:home|Users)\/|[A-Za-z]:\/Users\/)([^/\s"'<>`,;:?#&()[\]{}=]+)/gu,
    (match) => ({ offset: match[0].length - match[1].length, value: "<USER>" }),
  );
  return redactUsername(sanitized, username);
}

export interface SanitizeContext {
  projectRoots?: readonly string[];
  home?: string;
  username?: string;
}

/** Decode for matching only; retain source spans so unrelated text stays byte-for-byte intact. */
function decodedView(content: string) {
  let units = Array.from({ length: content.length }, (_, i) => ({
    char: content[i],
    start: i,
    end: i + 1,
  }));
  const decoded: typeof units = [];
  for (let i = 0; i < units.length; ) {
    const byte = content.slice(i, i + 3);
    const first = /^%[\da-f]{2}$/i.test(byte) ? Number.parseInt(byte.slice(1), 16) : -1;
    const width = first < 0x80 ? 1 : first >= 0xc2 && first <= 0xdf ? 2 : first <= 0xef ? 3 : 4;
    const spelling = content.slice(i, i + width * 3);
    if (first >= 0 && /^(?:%[\da-f]{2})+$/i.test(spelling)) {
      try {
        const value = decodeURIComponent(spelling);
        for (const char of value.split(""))
          decoded.push({ char, start: i, end: i + spelling.length });
        i += spelling.length;
        continue;
      } catch {
        // Malformed URL bytes are not paths; keep them unchanged.
      }
    }
    decoded.push(units[i++]);
  }
  units = [];
  for (let i = 0; i < decoded.length; i++) {
    const unit = decoded[i];
    const next = decoded[i + 1];
    if (unit.char === "\\" && next && (next.char === "/" || next.char === "\\")) {
      units.push({ char: "/", start: unit.start, end: next.end });
      i++;
    } else units.push({ ...unit, char: unit.char === "\\" ? "/" : unit.char });
  }
  return { text: units.map((unit) => unit.char).join(""), units };
}

function replaceDecoded(
  content: string,
  pattern: RegExp,
  replacement: (match: RegExpExecArray) => { offset: number; value: string },
): string {
  const view = decodedView(content);
  let result = "";
  let cursor = 0;
  for (const match of view.text.matchAll(pattern)) {
    const { offset, value } = replacement(match);
    const start = view.units[match.index + offset].start;
    const end = view.units[match.index + match[0].length - 1].end;
    result += content.slice(cursor, start) + value;
    cursor = end;
  }
  return result + content.slice(cursor);
}

function replacePath(content: string, path: string, value: string): string {
  const normalized = path.replace(/\\/g, "/").replace(/\/+$/, "");
  if (!normalized) return content;
  const pattern = new RegExp(
    `(?:(?<![\\p{L}\\p{N}_/.-])|(?<=file://)|(?<=file:///))${escapeRegex(normalized)}(?=$|[/\\s"'<>\x60,;:)\\]?#&])`,
    /^[A-Za-z]:/.test(path) ? "giu" : "gu",
  );
  return replaceDecoded(content, pattern, () => ({ offset: 0, value }));
}

/** Root metadata is authoritative; arbitrary absolute paths in commands are not project roots. */
function reportProjectRoots(content: string): string[] {
  return [
    ...content.matchAll(/(?:project root set: |watcher started: )([^\r\n]+)/g),
    ...content.matchAll(
      /(?:\broot=|"(?:projectRoot|project_root|canonical_root)"\s*:\s*)("[^"]+"|'[^']+'|[^\s"'<>`,;]+)/g,
    ),
  ]
    .map((match) => match[1].trim().replace(/^["']|["']$/g, ""))
    .filter((root) => /^(?:\/|[A-Za-z]:\/)/.test(root));
}

/**
 * Recursively sanitize any value by deep-walking objects/arrays. Strings are
 * passed through `sanitizeContent`; other primitives are preserved.
 */
export function sanitizeValue(value: unknown, context: SanitizeContext = {}): unknown {
  const scoped = {
    ...context,
    projectRoots: [...(context.projectRoots ?? []), ...projectRootsFromValue(value)],
  };
  return sanitizeNestedValue(value, scoped);
}

function sanitizeNestedValue(value: unknown, context: SanitizeContext): unknown {
  if (typeof value === "string") {
    return sanitizeContent(value, context);
  }
  if (Array.isArray(value)) {
    return value.map((entry) => sanitizeNestedValue(entry, context));
  }
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, entry]) => [key, sanitizeNestedValue(entry, context)]),
    );
  }
  return value;
}

/** Collect only declared root fields, never arbitrary file/config/cache paths. */
export function projectRootsFromValue(value: unknown): string[] {
  if (!value || typeof value !== "object") return [];
  if (Array.isArray(value)) return value.flatMap(projectRootsFromValue);
  const object = value as Record<string, unknown>;
  const roots = [object.projectRoot, object.project_root, object.canonical_root].filter(
    (root): root is string => typeof root === "string",
  );
  const index = object.index as { roots?: { path?: unknown }[] } | undefined;
  if (index && Array.isArray(index.roots)) {
    for (const root of index.roots)
      if (root && typeof root.path === "string") roots.push(root.path);
  }
  return [...roots, ...Object.values(object).flatMap(projectRootsFromValue)];
}

/** SSH identities and shell home expansion are username positions, unlike bare words or /dev/. */
export function redactUsername(content: string, username: string): string {
  if (!username) return content;
  return replaceDecoded(
    content,
    new RegExp(
      `(?<![\\p{L}\\p{N}_/.-])~${escapeRegex(username)}(?![\\p{L}\\p{N}_])|(?<![\\p{L}\\p{N}_/.-])${escapeRegex(username)}(?=@[\\p{L}\\p{N}])`,
      "gu",
    ),
    (match) => ({ offset: match[0].startsWith("~") ? 1 : 0, value: "<USER>" }),
  );
}
