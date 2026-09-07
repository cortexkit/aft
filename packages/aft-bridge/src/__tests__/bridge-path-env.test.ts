/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { BinaryBridge } from "../bridge.js";

let workDir: string;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-bridge-path-env-"));
});

afterEach(() => {
  rmSync(workDir, { recursive: true, force: true });
});

function writeEnvReportingBridge(outputPath: string): string {
  const fixturePath = join(workDir, "env-reporting-bridge.cjs");
  writeFileSync(
    fixturePath,
    `#!${process.execPath}
const { writeFileSync } = require("node:fs");
writeFileSync(${JSON.stringify(outputPath)}, JSON.stringify(process.env));
process.stdin.setEncoding("utf8");
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newline;
  while ((newline = buffer.indexOf("\\n")) !== -1) {
    const request = JSON.parse(buffer.slice(0, newline));
    buffer = buffer.slice(newline + 1);
    process.stdout.write(JSON.stringify({ id: request.id, success: true }) + "\\n");
  }
});
`,
  );
  chmodSync(fixturePath, 0o755);
  return fixturePath;
}

describe("BinaryBridge child PATH", () => {
  test("passes one inherited path key with the managed ONNX directory prepended on Windows", async () => {
    const outputPath = join(workDir, "child-env.json");
    const fixturePath = writeEnvReportingBridge(outputPath);
    const bridge = new BinaryBridge(
      fixturePath,
      workDir,
      {
        maxRestarts: 0,
        childEnv: {
          PATH: undefined,
          Path: "C:\\Windows\\System32;C:\\Git\\cmd",
        },
      },
      { _ort_dylib_dir: "C:\\onnxruntime" },
      undefined,
      "win32",
    );

    try {
      await bridge.send("status");
      const childEnv = JSON.parse(readFileSync(outputPath, "utf8")) as Record<string, string>;
      const pathKeys = Object.keys(childEnv).filter((key) => key.toLowerCase() === "path");

      expect(pathKeys).toEqual(["Path"]);
      expect(childEnv.Path).toBe("C:\\onnxruntime;C:\\Windows\\System32;C:\\Git\\cmd");
    } finally {
      await bridge.shutdown();
    }
  });
});
