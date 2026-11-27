import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { e2eBinaryRequired, stagedE2eBinary } from "../../../../tests/helpers/e2e-binary.js";

describe("e2e binary requirement", () => {
  test("every CI spelling requires the binary", () => {
    for (const env of [
      { CI: "true" },
      { CI: "TRUE" },
      { CI: "1" },
      { GITHUB_ACTIONS: "true" },
      { AFT_REQUIRE_E2E_BINARY: "1" },
    ]) {
      expect(e2eBinaryRequired(env)).toBe(true);
    }
  });

  test("a local run without a CI flag may skip", () => {
    expect(e2eBinaryRequired({})).toBe(false);
    expect(e2eBinaryRequired({ CI: "false" })).toBe(false);
    expect(e2eBinaryRequired({ CI: "" })).toBe(false);
  });

  test("an invalid explicit path fails outside CI instead of selecting another aft", () => {
    expect(() => stagedE2eBinary({ AFT_BINARY_PATH: `${process.execPath}/missing` })).toThrow(
      "AFT_BINARY_PATH is not an executable file",
    );
    expect(stagedE2eBinary({ AFT_BINARY_PATH: " " })).toBeNull();
  });

  test("OpenCode uses the staged binary even if the Cargo target already has aft", () => {
    const result = prepareWithDebugCandidate(` ${process.execPath} `);
    expect(result.exitCode, result.stderr.toString()).toBe(0);
    expect(JSON.parse(result.stdout.toString())).toEqual({
      binaryPath: process.execPath,
      source: "AFT_BINARY_PATH",
      buildAttempted: false,
    });
  });

  test("OpenCode fails CI without a staged binary even if the Cargo target already has aft", () => {
    const result = prepareWithDebugCandidate("");
    expect(result.exitCode).toBe(1);
    expect(result.stderr.toString()).toContain("set AFT_BINARY_PATH to the aft binary staged");
  });
});

function prepareWithDebugCandidate(staged: string) {
  const target = mkdtempSync(join(tmpdir(), "aft-e2e-binary-"));
  try {
    const debug = join(target, "debug");
    mkdirSync(debug);
    const name = process.platform === "win32" ? "aft.exe" : "aft";
    writeFileSync(join(debug, name), "not the staged binary", { mode: 0o755 });
    const helper = join(import.meta.dir, "e2e/helpers.ts");
    return Bun.spawnSync(
      [
        process.execPath,
        "-e",
        `const { prepareBinary } = await import(${JSON.stringify(helper)});
         console.log(JSON.stringify(await prepareBinary()));`,
      ],
      { env: { ...process.env, AFT_BINARY_PATH: staged, CARGO_TARGET_DIR: target, CI: "true" } },
    );
  } finally {
    rmSync(target, { recursive: true, force: true });
  }
}
