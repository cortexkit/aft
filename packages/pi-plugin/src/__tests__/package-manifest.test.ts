import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { bundledTuiSpecifiers } from "../../scripts/check-bundle.mjs";

// Pi supplies these modules to every extension through its own module mapping
// (Pi's docs/packages.md, "Declare dependencies"). A copy installed from our
// `dependencies` can bypass that mapping and load a second instance of the
// host's classes and registries, so Pi warns at startup when it sees one. They
// must be `"*"` peers instead, and kept external in the bundle.
const HOST_PROVIDED = [
  "@earendil-works/pi-ai",
  "@earendil-works/pi-agent-core",
  "@earendil-works/pi-coding-agent",
  "@earendil-works/pi-tui",
  "typebox",
];

interface Manifest {
  dependencies?: Record<string, string>;
  devDependencies?: Record<string, string>;
  files?: string[];
  peerDependencies?: Record<string, string>;
  scripts?: Record<string, string>;
}

const manifest: Manifest = JSON.parse(
  readFileSync(join(import.meta.dir, "..", "..", "package.json"), "utf8"),
);

describe("package manifest", () => {
  test("never lists a Pi host-provided package in dependencies", () => {
    const dependencies = Object.keys(manifest.dependencies ?? {});
    expect(dependencies.filter((name) => HOST_PROVIDED.includes(name))).toEqual([]);
  });

  test("declares every host package the plugin imports as a * peer and keeps it external", () => {
    const build = manifest.scripts?.build ?? "";
    for (const name of ["@earendil-works/pi-ai", "@earendil-works/pi-coding-agent", "typebox"]) {
      expect(manifest.peerDependencies?.[name]).toBe("*");
      expect(build).toContain(`--external ${name}`);
    }
  });

  test("bundles Pi TUI as a dev dependency and ships its license notice", () => {
    const name = "@earendil-works/pi-tui";
    const version = manifest.devDependencies?.[name];
    expect(manifest.peerDependencies?.[name]).toBeUndefined();
    expect(manifest.scripts?.build).not.toContain(`--external ${name}`);
    expect(manifest.scripts?.build).toContain("scripts/check-bundle.mjs");
    expect(version).toMatch(/^\d+\.\d+\.\d+$/);
    expect(manifest.files).toContain("THIRD-PARTY-NOTICES.txt");
    const notices = readFileSync(join(import.meta.dir, "../../THIRD-PARTY-NOTICES.txt"), "utf8");
    expect(notices).toContain(`${name} ${version}`);
    expect(notices).toContain("Copyright (c) 2025 Mario Zechner");
  });

  test("the bundle guard detects static imports, re-exports, dynamic imports and require helpers", () => {
    expect(
      bundledTuiSpecifiers(`
        // node_modules/@earendil-works/pi-tui/dist/utils.js
        import { Text } from '@earendil-works/pi-tui';
        export { TUI } from '@oh-my-pi/pi-tui';
        import(\`@oh-my-pi/pi-tui/text/width\`);
        __require('@earendil-works/pi-tui/dist/index.js');
        require('@earendil-works/pi-coding-agent');
      `),
    ).toEqual([
      "@earendil-works/pi-tui",
      "@oh-my-pi/pi-tui",
      "@oh-my-pi/pi-tui/text/width",
      "@earendil-works/pi-tui/dist/index.js",
    ]);
    expect(
      bundledTuiSpecifiers(`
        // import '@earendil-works/pi-tui'
        let manifest = { devDependencies: { '@earendil-works/pi-tui': '0.80.2' } };
      `),
    ).toEqual([]);
  });
});
