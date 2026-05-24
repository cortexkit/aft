import { createRequire } from "node:module";

/**
 * Resolve this CLI package's own version. We try a couple of relative paths
 * because Bun's bundler places the runtime entry one or two levels away from
 * `package.json` depending on build config.
 */
export function getSelfVersion(): string {
  const require = createRequire(import.meta.url);
  for (const relPath of ["../../package.json", "../package.json"]) {
    try {
      const version = (require(relPath) as { version?: string }).version;
      if (typeof version === "string" && version.length > 0) {
        return version;
      }
    } catch {
      // next candidate
    }
  }
  return "unknown";
}
