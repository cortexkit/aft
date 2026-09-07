import { createRequire } from "node:module";

export const PLUGIN_PACKAGE_NAME = "@cortexkit/aft-opencode";

/**
 * Resolve the plugin's own version from the package manifest that names it.
 *
 * `index.ts` is bundled into more than one entry file at different depths
 * (`dist/index.js` for the V1 root export, `dist/entry/server.js` for the
 * `./server` export), so a fixed `../package.json` only resolves from one of
 * them: from the server entry it named `dist/package.json`, the read failed
 * silently, the plugin asked the resolver for binary v0.0.0, and the host
 * refused to load the plugin. Walk up from the caller's own location and stop
 * at the first manifest whose `name` is this package; an unrelated manifest
 * (a consumer's, or a monorepo root's) is skipped, not trusted.
 */
export function resolvePluginVersion(importMetaUrl: string): string {
  const req = createRequire(importMetaUrl);
  for (const candidate of ["../package.json", "../../package.json", "../../../package.json"]) {
    try {
      const manifest = req(candidate) as { name?: string; version?: string };
      if (manifest.name === PLUGIN_PACKAGE_NAME && manifest.version) {
        return manifest.version;
      }
    } catch {
      // Not at this depth; try the next.
    }
  }
  return "0.0.0";
}
