import { describe, expect, test } from "bun:test";
import {
  chmodSync,
  linkSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { PiAdapter } from "../adapters/pi.js";
import { platformKey } from "../lib/binary-probe.js";
import { dirSize, formatBytes } from "../lib/fs-util.js";
import { readJsoncFile, writeJsoncFile } from "../lib/jsonc.js";
import { clearLspCaches, getLspCacheReport } from "../lib/lsp-cache.js";

function fixture(fn: (root: string) => void): void {
  const root = mkdtempSync(join(tmpdir(), "aft-cli-regression-"));
  try {
    fn(root);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

describe("cache/config regressions", () => {
  test("json extension retains parsed inline and multiline comments", () => {
    fixture((root) => {
      const path = join(root, "opencode.json");
      writeFileSync(path, '/* line one\n line two */\n{ "enabled": true // inline\n}\n');
      const value = readJsoncFile(path).value;
      expect(value).not.toBeNull();
      if (!value) throw new Error("comment fixture did not parse");
      writeJsoncFile(path, value, "json");
      const raw = readFileSync(path, "utf8");
      expect(raw).toContain("line one");
      expect(raw).toContain("line two");
      expect(raw).toContain("// inline");
    });
  });

  test("config replacement preserves symlink and mode without modifying old inode", () => {
    fixture((root) => {
      const target = join(root, "target.json");
      const alias = join(root, "config.json");
      const old = join(root, "old-inode.json");
      writeFileSync(target, '{"old":true}\n');
      chmodSync(target, 0o640);
      linkSync(target, old);
      symlinkSync(target, alias);
      writeJsoncFile(alias, { updated: true }, "json");
      expect(lstatSync(alias).isSymbolicLink()).toBe(true);
      expect(JSON.parse(readFileSync(target, "utf8"))).toEqual({ updated: true });
      expect(readFileSync(old, "utf8")).toBe('{"old":true}\n');
      expect(statSync(target).mode & 0o777).toBe(0o640);
      expect(readdirSync(root).sort()).toEqual(["config.json", "old-inode.json", "target.json"]);
    });
  });

  test("Windows ARM64 resolves its published platform package key", () => {
    expect(platformKey("win32", "arm64")).toBe("win32-arm64");
    expect(platformKey("linux", "mips")).toBeNull();
  });

  test("dangling config links are refused without replacing the link", () => {
    fixture((root) => {
      const target = join(root, "missing", "target.json");
      const alias = join(root, "config.json");
      symlinkSync(target, alias);
      const linkTarget = readlinkSync(alias);
      expect(() => writeJsoncFile(alias, { updated: true }, "json")).toThrow(/symlink target/);
      expect(lstatSync(alias).isSymbolicLink()).toBe(true);
      expect(readlinkSync(alias)).toBe(linkTarget);
      expect(readdirSync(root)).toEqual(["config.json"]);
    });
  });

  test("malformed encoded cache names remain reportable and clearable", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-cache-name-"));
    try {
      await withEnv({ AFT_CACHE_DIR: root }, () => {
        const entry = join(root, "lsp-packages", "broken%pkg");
        mkdirSync(entry, { recursive: true });
        writeFileSync(join(entry, "payload"), "payload");
        expect(getLspCacheReport().npm.entries.map((item) => item.name)).toEqual(["broken%pkg"]);
        expect(clearLspCaches().cleared.map((item) => item.name)).toEqual(["broken%pkg"]);
      });
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("Pi recognizes percent-encoded file URLs and exposes its configured entry", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-pi-url-"));
    try {
      const pkg = join(root, "local package");
      mkdirSync(pkg);
      writeFileSync(join(pkg, "package.json"), JSON.stringify({ name: "@cortexkit/aft-pi" }));
      const entry = pathToFileURL(pkg).href;
      writeFileSync(join(root, "settings.json"), JSON.stringify({ packages: [entry] }));
      await withEnv({ PI_CODING_AGENT_DIR: root }, () => {
        const adapter = new PiAdapter();
        expect(adapter.hasPluginEntry()).toBe(true);
        expect(adapter.getConfiguredPluginEntry()).toBe(entry);
      });
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("Pi empty version output is absent rather than an empty version", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-pi-version-"));
    try {
      const executable = join(root, "pi");
      writeFileSync(executable, "#!/bin/sh\nexit 0\n", { mode: 0o755 });
      await withEnv({ PATH: `${root}:${process.env.PATH}` }, () => {
        expect(new PiAdapter().getHostVersion()).toBeNull();
      });
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("Pi recognizes absolute plugin paths", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-pi-path-"));
    try {
      const pkg = join(root, "local-package");
      mkdirSync(pkg);
      writeFileSync(join(pkg, "package.json"), JSON.stringify({ name: "@cortexkit/aft-pi" }));
      writeFileSync(join(root, "settings.json"), JSON.stringify({ packages: [pkg] }));
      await withEnv({ PI_CODING_AGENT_DIR: root }, () => {
        expect(new PiAdapter().hasPluginEntry()).toBe(true);
      });
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("physical cache size excludes symlinks and directory cycles", () => {
    fixture((root) => {
      writeFileSync(join(root, "payload"), "four");
      symlinkSync(root, join(root, "cycle"));
      symlinkSync(join(root, "payload"), join(root, "duplicate"));
      expect(dirSize(root)).toBe(4);
      expect(dirSize(join(root, "duplicate"))).toBe(0);
    });
  });

  test("byte formatting guards invalid small and oversized inputs", () => {
    for (const bytes of [-1, 0, 0.5, Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(formatBytes(bytes)).toBe("0 B");
    }
    expect(formatBytes(1)).toBe("1.0 B");
    expect(formatBytes(1024)).toBe("1.0 KB");
    expect(formatBytes(1024 ** 5)).toBe("1024.0 TB");
  });
});
