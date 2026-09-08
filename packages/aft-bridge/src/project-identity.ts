import { createHash } from "node:crypto";
import { realpathSync } from "node:fs";
import { resolve } from "node:path";

/**
 * The single TypeScript project-root canonicalizer, mirroring the Rust
 * `cortexkit-paths` `ProjectRootId`: resolve symlinks (`realpath`), strip
 * trailing separators, normalize Windows verbatim/UNC prefixes, and uppercase
 * the drive letter. Falls back to lexical resolution for paths that don't
 * exist (so callers that canonicalize a not-yet-created or transient path stay
 * total instead of throwing).
 *
 * Why one canonicalizer: AFT used to derive project-root identity four
 * different ways across the TS layer — bridge routing realpath'd, but RPC
 * port-file scoping (`projectHash`) and the sidebar status gate compared raw
 * strings. That divergence is the bug behind sidebar-shows-wrong-project and
 * stale-port discovery: a symlinked / raw-spelled launch dir hashed to a
 * different port directory than the bridge routed to.
 *
 * TS↔Rust *pixel* parity is NOT required (under the daemon, subc and AFT
 * re-canonicalize the received root authoritatively). What IS required is
 * TS-internal self-consistency: every routing, scoping, and port-file site
 * routes through THIS function, so the bridge routing key and the RPC port
 * scope always agree for the same project.
 */
export function canonicalizeProjectRoot(dir: string): string {
  const trimmed = dir.replace(/[/\\]+$/, "");
  let canonical: string;
  try {
    canonical = realpathSync(trimmed);
  } catch {
    canonical = resolve(trimmed);
  }
  return normalizeWindowsRoot(canonical);
}

/**
 * Strip only safely convertible Windows extended-length DOS and UNC prefixes,
 * and uppercase a lowercase drive letter so `c:\x` and `C:\x` collapse to one
 * identity. Namespaces such as `\\?\Volume{GUID}\` must retain their prefix.
 * `platform` is injectable for cross-platform regression tests. No-op off Windows.
 */
export function normalizeWindowsRoot(p: string, platform = process.platform): string {
  if (platform !== "win32") return p;
  let s = p;
  const lower = s.toLowerCase();
  const uncPrefix = ["\\\\?\\unc\\", "\\\\??\\unc\\", "\\??\\unc\\"].find((prefix) =>
    lower.startsWith(prefix),
  );
  if (uncPrefix) {
    const tail = s.slice(uncPrefix.length);
    if (/^[^\\/]+[\\/][^\\/]+(?:[\\/]|$)/.test(tail) && !hasDotComponent(tail)) {
      s = `\\\\${tail}`;
    }
  } else {
    const dosPrefix = ["\\\\?\\", "\\\\??\\", "\\??\\"].find((prefix) => lower.startsWith(prefix));
    if (dosPrefix) {
      const tail = s.slice(dosPrefix.length);
      if (/^[a-z]:[\\/]/i.test(tail) && !hasDotComponent(tail)) s = tail;
    }
  }
  if (s.length >= 2 && s[1] === ":") {
    const drive = s.charCodeAt(0);
    if (drive >= 97 && drive <= 122) {
      s = s[0].toUpperCase() + s.slice(1);
    }
  }
  return s;
}

function hasDotComponent(path: string): boolean {
  return path.split(/[\\/]/).some((component) => component === "." || component === "..");
}

/**
 * Stable 16-hex scope hash of the canonical project root. Used for RPC
 * port-file directory scoping; because it canonicalizes first, the server
 * writing a port file and the client discovering it agree on the directory
 * even when one was handed a symlinked or raw-spelled path.
 */
export function projectRootKeyHash(dir: string): string {
  return createHash("sha256").update(canonicalizeProjectRoot(dir)).digest("hex").slice(0, 16);
}
