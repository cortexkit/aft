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

const quotedSensitiveKeyValuePattern = new RegExp(
  String.raw`((['"])(${KEY_NAME})\2[^\S\r\n]*:[^\S\r\n]*)(['"])([^'"\r\n]+)\4`,
  "gi",
);
const unquotedSensitiveKeyValuePattern = new RegExp(
  String.raw`\b((${KEY_NAME})[^\S\r\n]*[=:][^\S\r\n]*)(['"])([^'"\r\n]+)\3`,
  "gi",
);
const bareSensitiveKeyValuePattern = new RegExp(
  String.raw`\b((${KEY_NAME})[^\S\r\n]*[=:][^\S\r\n]*)([^\s,;&'"]+)`,
  "gi",
);

function isSensitiveKeyName(keyName: string): boolean {
  return (
    SENSITIVE_KEY_WORD.test(keyName) ||
    SEGMENTED_KEY_WORD.test(keyName) ||
    CAMEL_CASE_KEY_WORD.test(keyName)
  );
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
  sanitized = sanitized.replace(
    unquotedSensitiveKeyValuePattern,
    (match: string, prefix: string, keyName: string, valueQuote: string) =>
      isSensitiveKeyName(keyName)
        ? `${prefix}${valueQuote}${SECRET_PLACEHOLDER}${valueQuote}`
        : match,
  );
  sanitized = sanitized.replace(
    bareSensitiveKeyValuePattern,
    (match: string, prefix: string, keyName: string) =>
      isSensitiveKeyName(keyName) ? `${prefix}${SECRET_PLACEHOLDER}` : match,
  );
  sanitized = sanitized.replace(
    /\b([a-z][a-z0-9+.-]*:\/\/)[^@\s/?#]+@/gi,
    `$1${URL_CREDENTIAL_PLACEHOLDER}@`,
  );
  sanitized = sanitized.replace(/\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi, "<EMAIL>");

  return sanitized;
}

/**
 * Strip personally identifiable path segments and usernames from arbitrary
 * text. Used on log contents, diagnostic JSON blocks, and the final issue body
 * so reports never leak usernames or home-directory paths.
 */
export function sanitizeContent(content: string): string {
  const username = userInfo().username;
  const home = homedir();

  let sanitized = redactSecrets(content);

  // Redact the project/working-directory prefix first. It's the most specific
  // path and often the biggest leak in logs (it names the repo the user is
  // working on). Done before the home-dir pass because the cwd usually lives
  // under home; in-project relative structure is left intact for debugging.
  const cwd = process.cwd();
  for (const candidate of new Set([cwd, safeRealpath(cwd)])) {
    if (candidate && candidate !== "/" && candidate !== home) {
      sanitized = sanitized.replace(new RegExp(escapeRegex(candidate), "g"), "<PROJECT>");
    }
  }

  if (home) {
    sanitized = sanitized.replace(new RegExp(escapeRegex(home), "g"), "~");
  }
  sanitized = sanitized.replace(/\/Users\/[^/\s"']+/g, "/Users/<USER>");
  sanitized = sanitized.replace(/\/home\/[^/\s"']+/g, "/home/<USER>");
  // Windows user dirs, any drive letter, both slash styles, and escaped-backslash
  // (JSON-encoded) forms. The home/username passes only catch the ACTIVE user;
  // a log can carry other users' paths or a non-C: drive (e.g. a CI runner or a
  // pasted path), which would otherwise leak into the public issue body. The
  // drive letter is preserved (not PII); only the username segment is redacted.
  sanitized = sanitized.replace(/([A-Za-z]:\\\\Users\\\\)[^\\\\"'\s]+/g, "$1<USER>");
  sanitized = sanitized.replace(/([A-Za-z]:\\Users\\)[^\\"'\s]+/g, "$1<USER>");
  sanitized = sanitized.replace(/([A-Za-z]:\/Users\/)[^/\s"']+/g, "$1<USER>");
  if (username) {
    sanitized = sanitized.replace(new RegExp(escapeRegex(username), "g"), "<USER>");
  }
  return sanitized;
}

/**
 * Recursively sanitize any value by deep-walking objects/arrays. Strings are
 * passed through `sanitizeContent`; other primitives are preserved.
 */
export function sanitizeValue(value: unknown): unknown {
  if (typeof value === "string") {
    return sanitizeContent(value);
  }
  if (Array.isArray(value)) {
    return value.map((entry) => sanitizeValue(entry));
  }
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, entry]) => [key, sanitizeValue(entry)]),
    );
  }
  return value;
}
