import { expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { OpenCodeAdapter } from "../adapters/opencode.js";
import { PiAdapter } from "../adapters/pi.js";
import { clearOldBinaries } from "../commands/doctor.js";
import { getSelfVersion } from "../lib/self-version.js";

for (const kind of ["OpenCode", "Pi"] as const) {
  test(`${kind} unreadable installed manifest preserves cached binaries through the real adapter`, async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-unreadable-plugin-"));
    try {
      const config = join(root, "config");
      const pi = join(root, "pi");
      mkdirSync(config);
      mkdirSync(pi);
      writeFileSync(
        join(config, "opencode.json"),
        JSON.stringify({ plugin: ["@cortexkit/aft-opencode"] }),
      );
      writeFileSync(
        join(pi, "settings.json"),
        JSON.stringify({ packages: ["npm:@cortexkit/aft-pi"] }),
      );
      await withEnv(
        {
          OPENCODE_CONFIG_DIR: config,
          PI_CODING_AGENT_DIR: pi,
          XDG_CACHE_HOME: join(root, "cache"),
          AFT_CACHE_DIR: join(root, "aft-cache"),
        },
        () => {
          const adapter = kind === "OpenCode" ? new OpenCodeAdapter() : new PiAdapter();
          const info = adapter.getPluginCacheInfo();
          if (adapter instanceof OpenCodeAdapter) {
            expect(info.configuredEntry).toBe("@cortexkit/aft-opencode");
          } else {
            expect(adapter.getConfiguredPluginEntry()).toBe("npm:@cortexkit/aft-pi");
          }
          const manifest =
            kind === "OpenCode"
              ? join(info.path, "node_modules", "@cortexkit", "aft-opencode", "package.json")
              : info.path;
          // A directory is unreadable as a manifest even when tests run as root.
          mkdirSync(manifest, { recursive: true });
          const binary = join(root, "aft-cache", "bin", "v0.1.0", "aft");
          mkdirSync(join(binary, ".."), { recursive: true });
          writeFileSync(binary, "offline plugin binary");
          const result = clearOldBinaries(getSelfVersion(), [adapter]);
          expect(result.cleared).toBe(0);
          expect(existsSync(binary)).toBe(true);
          expect(adapter.getPluginCacheInfo().readError).toBeDefined();
        },
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
}
