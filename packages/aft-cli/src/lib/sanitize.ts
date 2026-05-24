import { homedir, userInfo } from "node:os";

export function escapeRegex(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Strip personally identifiable path segments and usernames from arbitrary
 * text. Used on log contents, diagnostic JSON blocks, and the final issue body
 * so reports never leak usernames or home-directory paths.
 */
export function sanitizeContent(content: string): string {
  const username = userInfo().username;
  const home = homedir();

  let sanitized = content;
  if (home) {
    sanitized = sanitized.replace(new RegExp(escapeRegex(home), "g"), "~");
  }
  sanitized = sanitized.replace(/\/Users\/[^/\s"']+/g, "/Users/<USER>");
  sanitized = sanitized.replace(/\/home\/[^/\s"']+/g, "/home/<USER>");
  sanitized = sanitized.replace(/C:\\\\Users\\\\[^\\\\"'\s]+/g, "C:\\\\Users\\\\<USER>");
  sanitized = sanitized.replace(/C:\\Users\\[^\\"'\s]+/g, "C:\\Users\\<USER>");
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
