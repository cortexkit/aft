import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "../child-process.js";
import { lspNpmInstallInvocation } from "../lsp-npm-policy.js";
import { LSP_NPM_REFUSAL, prepareLspNpmInstall, selectLspNpm } from "../lsp-npm-selection.js";
import { npmInvocation, npmSpawnEnv, resolveNpm } from "../npm-resolver.js";

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function wrapper(version: string) {
  const root = mkdtempSync(join(tmpdir(), "aft-npm-identity-"));
  roots.push(root);
  const command = join(root, "npm");
  writeFileSync(
    command,
    `#!/bin/sh\nif [ "$1" = "--version" ]; then echo '${version}'; exit 0; fi\necho 'rejected option: --no-save or --before' >&2\nexit 2\n`,
    { mode: 0o755 },
  );
  return { command, binDir: root };
}

describe("protected LSP npm selection", () => {
  test("genuine npm returns before fallback discovery or iteration", async () => {
    const npm = wrapper("11.6.0");
    writeFileSync(
      join(npm.binDir, "package.json"),
      JSON.stringify({ name: "npm", version: "11.6.0", bin: { npm: "npm" } }),
    );
    const fallback: (typeof npm)[] = [];
    Object.defineProperty(fallback, Symbol.iterator, {
      value: () => {
        throw new Error("fallback must not run");
      },
    });
    expect((await selectLspNpm(npm, fallback)).npm?.command).toBe(npm.command);
  });

  test("shutdown during preferred-npm verification does not start fallback discovery", async () => {
    const npm = wrapper("11.6.0");
    const controller = new AbortController();
    const fallback: (typeof npm)[] = [];
    Object.defineProperty(fallback, Symbol.iterator, {
      value: () => {
        throw new Error("fallback must not run after shutdown");
      },
    });
    const pending = selectLspNpm(npm, fallback, controller.signal);
    controller.abort();
    expect((await pending).refusal).toContain("installation aborted");
  });
  test("direct Node invocations cache capability by npm CLI identity, not the Node executable", async () => {
    const npm = wrapper("11.6.0");
    const cliPath = join(npm.binDir, "npm.mjs");
    const otherCli = join(npm.binDir, "other-npm.mjs");
    writeFileSync(cliPath, 'console.log("11.6.0");\n');
    writeFileSync(otherCli, 'console.log("6.14.0");\n');
    const direct = { command: process.execPath, cliPath, binDir: null };
    const before = "2020-01-01T00:00:00.000Z";
    expect(await lspNpmInstallInvocation(direct, "fixture", before)).not.toBeNull();
    expect(
      await lspNpmInstallInvocation({ ...direct, cliPath: otherCli }, "fixture", before),
    ).toBeNull();
    writeFileSync(cliPath, 'console.log("6.14.0");\n');
    const changed = new Date(Date.now() + 1000);
    utimesSync(cliPath, changed, changed);
    expect(await lspNpmInstallInvocation(direct, "fixture", before)).toBeNull();
  });
  for (const version of ["pnpm/11.1.2", "11.1.2"]) {
    for (const pinned of [false, true]) {
      test(`refuses ${version} wrapper for ${pinned ? "pinned" : "unpinned"} installs`, async () => {
        const npm = wrapper(version);
        const selected = await prepareLspNpmInstall(
          npm,
          "fixture@1.0.0",
          pinned ? undefined : "2020-01-01T00:00:00.000Z",
          [],
        );
        expect(selected.npm).toBeUndefined();
        expect(selected.refusal).toBe(
          LSP_NPM_REFUSAL.replace("<path>", npm.command).replace(
            "<reason>",
            "not npm: no matching npm package manifest and CLI",
          ),
        );
        // The numeric wrapper would have passed the old version-only check.
        const oldCheck = npmInvocation(npm, ["--version"]);
        expect(spawnSync(oldCheck.command, oldCheck.args, { encoding: "utf8" }).stdout.trim()).toBe(
          version,
        );
      });
    }
  }

  test("refuses old npm and missing CLI with their paths and reasons", async () => {
    const npm = wrapper("6.14.0");
    writeFileSync(
      join(npm.binDir, "package.json"),
      JSON.stringify({ name: "npm", version: "6.14.0", bin: { npm: "npm" } }),
    );
    expect((await selectLspNpm(npm, [])).refusal).toContain("old npm: 6.14.0");
    const missing = { command: join(npm.binDir, "missing"), binDir: npm.binDir };
    expect((await selectLspNpm(missing, [])).refusal).toContain("probe failed:");
  });

  test("falls back from a rejecting wrapper to genuine npm without losing safety options", async () => {
    const command = process.env.AFT_TEST_REAL_NPM ?? resolveNpm()?.command;
    if (!command) throw new Error("AFT_TEST_REAL_NPM must identify genuine npm for this test");
    const rejected = wrapper("11.1.2");
    const selected = await selectLspNpm(rejected, [{ command, binDir: null }]);
    expect(selected.npm).toBeDefined();
    expect(selected.npm?.command).not.toBe(rejected.command);
    for (const before of [undefined, "2020-01-01T00:00:00.000Z"]) {
      const invocation = await lspNpmInstallInvocation(selected.npm!, "fixture@1.0.0", before);
      expect(invocation?.args).toContain("--ignore-scripts");
      expect(invocation?.args).toContain("--no-save");
      expect(invocation?.args.some((arg) => arg.startsWith("--before="))).toBe(Boolean(before));
    }
    const version = npmInvocation(selected.npm!, ["--version"]);
    expect(
      spawnSync(version.command, version.args, {
        encoding: "utf8",
        env: npmSpawnEnv(selected.npm!),
      }).status,
    ).toBe(0);
  });
});
