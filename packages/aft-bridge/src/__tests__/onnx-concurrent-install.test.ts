/// <reference path="../bun-test.d.ts" />

/**
 * Concurrent first-start installs of the managed ONNX Runtime.
 *
 * An OpenCode process can initialise the plugin more than once at the same
 * time, and two OpenCode windows can start together. Every one of those
 * starts asks for the runtime. The install must happen once, each waiting
 * caller must end up with the installed runtime (not a "skipped" null), and
 * no attempt may break another. The archive is served from a local HTTP
 * server so the real download, extract, copy and publish path runs: `fetch`
 * is wrapped to send the GitHub release URL to that server instead.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { execFileSync, spawn } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { __onnxTest__, getOnnxRuntimeInstallFailure } from "../index.js";

const { resolveOnnxRuntime, ORT_VERSION } = __onnxTest__;

const LIB_NAME = "libonnxruntime.so";
const ASSET_NAME = `onnxruntime-fake-${ORT_VERSION}`;
const PLATFORM_INFO = { assetName: ASSET_NAME, libName: LIB_NAME, archiveType: "tgz" as const };

let workDir: string;
let server: Server;
let archiveUrl: string;
let requests = 0;
const realFetch = globalThis.fetch;

/** Send ONNX Runtime release downloads to `target`; pass everything else through. */
function routeReleaseDownloadsTo(target: string): typeof fetch {
  return ((input: string | URL | Request, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    if (url.startsWith("https://github.com/microsoft/onnxruntime/releases/download/")) {
      return realFetch(target, init);
    }
    return realFetch(input, init);
  }) as typeof fetch;
}

function buildFakeArchive(dir: string): Buffer {
  const pkgRoot = join(dir, "pkg");
  const libDir = join(pkgRoot, ASSET_NAME, "lib");
  mkdirSync(libDir, { recursive: true });
  writeFileSync(join(libDir, LIB_NAME), "fake onnx runtime library");
  const archivePath = join(dir, "archive.tgz");
  execFileSync("tar", ["czf", archivePath, "-C", pkgRoot, ASSET_NAME]);
  return readFileSync(archivePath);
}

beforeEach(async () => {
  workDir = mkdtempSync(join(tmpdir(), "aft-onnx-concurrent-"));
  const archive = buildFakeArchive(workDir);
  requests = 0;
  server = createServer((_req, res) => {
    requests++;
    // Hold the response briefly so concurrent attempts overlap the download.
    setTimeout(() => {
      res.writeHead(200, { "content-length": String(archive.length) });
      res.end(archive);
    }, 300);
  });
  await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
  const { port } = server.address() as AddressInfo;
  archiveUrl = `http://127.0.0.1:${port}/archive.tgz`;
  globalThis.fetch = routeReleaseDownloadsTo(archiveUrl);
});

afterEach(async () => {
  globalThis.fetch = realFetch;
  await new Promise<void>((done) => server.close(() => done()));
  rmSync(workDir, { recursive: true, force: true });
});

function seams() {
  return {
    platformInfo: PLATFORM_INFO,
    systemSearchPaths: [],
    lockPollMs: 50,
  };
}

function expectInstalled(storageDir: string, result: string | null): void {
  const ortDir = join(storageDir, "onnxruntime", ORT_VERSION);
  expect(result).toBe(ortDir);
  expect(readFileSync(join(ortDir, LIB_NAME), "utf8")).toBe("fake onnx runtime library");
  // No staging dir, backup dir or lock is left behind.
  expect(readdirSync(join(storageDir, "onnxruntime")).sort()).toEqual([ORT_VERSION]);
}

/** Run one install in a separate process, the way a second OpenCode window would. */
function installInChildProcess(storageDir: string): Promise<string | null> {
  const modulePath = resolve(import.meta.dir, "../onnx-runtime.ts");
  const script = `
    const realFetch = globalThis.fetch;
    globalThis.fetch = (input, init) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
      return url.startsWith("https://github.com/microsoft/onnxruntime/releases/download/")
        ? realFetch(${JSON.stringify(archiveUrl)}, init)
        : realFetch(input, init);
    };
    const { __test__ } = await import(${JSON.stringify(modulePath)});
    const result = await __test__.resolveOnnxRuntime(${JSON.stringify(storageDir)}, ${JSON.stringify(seams())});
    process.stdout.write("RESULT=" + JSON.stringify(result) + "\\n");
  `;
  return new Promise((done, fail) => {
    const child = spawn(process.execPath, ["-e", script], { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });
    child.on("error", fail);
    child.on("close", (code) => {
      const line = stdout.split("\n").find((l) => l.startsWith("RESULT="));
      if (code !== 0 || !line) {
        fail(new Error(`child install exited ${code}: ${stderr}`));
        return;
      }
      done(JSON.parse(line.slice("RESULT=".length)) as string | null);
    });
  });
}

describe("concurrent ONNX Runtime installs", () => {
  test("a single install publishes the runtime", async () => {
    const storageDir = join(workDir, "storage-single");
    const result = await resolveOnnxRuntime(storageDir, seams());

    expectInstalled(storageDir, result);
    expect(requests).toBe(1);
  });

  test("two installs in one process both get the runtime from a single download", async () => {
    const storageDir = join(workDir, "storage");
    const [first, second] = await Promise.all([
      resolveOnnxRuntime(storageDir, seams()),
      resolveOnnxRuntime(storageDir, seams()),
    ]);

    expectInstalled(storageDir, first);
    expectInstalled(storageDir, second);
    expect(requests).toBe(1);
    expect(getOnnxRuntimeInstallFailure()).toBeNull();
  });

  test("installs racing from two processes both get the runtime from a single download", async () => {
    const storageDir = join(workDir, "storage-two-processes");
    const [inProcess, child] = await Promise.all([
      resolveOnnxRuntime(storageDir, seams()),
      installInChildProcess(storageDir),
    ]);

    expectInstalled(storageDir, inProcess);
    expect(child).toBe(join(storageDir, "onnxruntime", ORT_VERSION));
    expect(requests).toBe(1);
  }, 30_000);

  test("a failed install reports why", async () => {
    const storageDir = join(workDir, "storage-failing");
    // Port 1 has no listener, so the download itself fails.
    globalThis.fetch = routeReleaseDownloadsTo("http://127.0.0.1:1/archive.tgz");
    const result = await resolveOnnxRuntime(storageDir, seams());

    expect(result).toBeNull();
    expect(getOnnxRuntimeInstallFailure()).toContain("ONNX Runtime download failed");
    expect(existsSync(join(storageDir, "onnxruntime", ORT_VERSION))).toBe(false);
  });
});
