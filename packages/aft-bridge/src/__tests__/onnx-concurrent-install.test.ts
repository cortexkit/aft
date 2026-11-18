/// <reference path="../bun-test.d.ts" />

/**
 * Concurrent first-start installs of the managed ONNX Runtime.
 *
 * An OpenCode process can initialise the plugin more than once at the same
 * time, and two OpenCode windows can start together. Every one of those
 * starts asks for the runtime. The install must happen once, each waiting
 * caller must end up with the installed runtime (not a "skipped" null), and
 * no attempt may break another. A local HTTP server serves a NuGet fixture so
 * the real download, ZIP reader, and cache publication path runs.
 */

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { deflateRawSync } from "node:zlib";
import * as bridge from "../index.js";

const { resolveOnnxRuntime, ORT_VERSION } = bridge.__onnxTest__;

/** Read through the namespace so a missing export fails only the tests that use it. */
const getOnnxRuntimeInstallFailure = (): string | null => bridge.getOnnxRuntimeInstallFailure();

const LIB_NAME = "onnxruntime.dll";
const RID = "win-x64";
const PACKAGE_URL = `https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/${ORT_VERSION}/microsoft.ml.onnxruntime.${ORT_VERSION}.nupkg`;
const FAKE_LIBRARY = "fake onnx runtime library";
const PLATFORM_INFO = {
  rid: RID,
  libName: LIB_NAME,
  librarySha256: createHash("sha256").update(FAKE_LIBRARY).digest("hex"),
};

let workDir: string;
let server: Server;
let archiveUrl: string;
let archive: ReturnType<typeof buildFakeArchive>;
let mode: "full" | "range" | "short" | "oversized";
let requests: string[];
let bytesSent: number;
const realFetch = globalThis.fetch;

function routePackageDownloadsTo(target: string): typeof fetch {
  return ((input: string | URL | Request, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    if (url === PACKAGE_URL) {
      return realFetch(target, init);
    }
    return realFetch(input, init);
  }) as typeof fetch;
}

function crc32(bytes: Buffer): number {
  let state = 0xffffffff;
  for (const byte of bytes) {
    state ^= byte;
    for (let i = 0; i < 8; i++) state = (state >>> 1) ^ (state & 1 ? 0xedb88320 : 0);
  }
  return (state ^ 0xffffffff) >>> 0;
}

function buildFakeArchive(method = 8, flags = 0) {
  const files: Buffer[] = [];
  const central: Buffer[] = [];
  let offset = 0;
  let libraryEnd = 0;
  let headerEnd = 0;
  for (const [path, data, compression, entryFlags] of [
    [`runtimes/${RID}/native/${LIB_NAME}`, Buffer.from(FAKE_LIBRARY), method, flags],
    ["runtimes/unrelated/native/unused.so", Buffer.alloc(128 * 1024), 0, 0],
  ] as const) {
    const name = Buffer.from(path);
    const payload = compression === 8 ? deflateRawSync(data) : data;
    const crc = crc32(data);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(entryFlags, 6);
    local.writeUInt16LE(compression, 8);
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(payload.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(name.length, 26);
    const entry = Buffer.alloc(46);
    entry.writeUInt32LE(0x02014b50);
    entry.writeUInt16LE(20, 4);
    local.copy(entry, 6, 4, 30);
    entry.writeUInt32LE(offset, 42);
    files.push(local, name, payload);
    central.push(entry, name);
    if (offset === 0) headerEnd = local.length;
    offset += local.length + name.length + payload.length;
    if (files.length === 3) libraryEnd = offset;
  }
  const directory = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50);
  end.writeUInt16LE(2, 8);
  end.writeUInt16LE(2, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return {
    bytes: Buffer.concat([...files, directory, end]),
    centralStart: offset,
    centralEnd: offset + directory.length,
    headerEnd,
    libraryEnd,
  };
}

beforeEach(async () => {
  workDir = mkdtempSync(join(tmpdir(), "aft-onnx-concurrent-"));
  archive = buildFakeArchive();
  mode = "full";
  requests = [];
  bytesSent = 0;
  server = createServer((req, res) => {
    const range = req.headers.range ?? "";
    requests.push(range);
    const { bytes } = archive;
    if (mode === "full" || mode === "oversized") {
      // Hold the response briefly so concurrent attempts overlap the download.
      setTimeout(() => {
        if (mode === "oversized") {
          res.writeHead(200, { "content-length": 201 * 1024 * 1024 });
          res.end();
          return;
        }
        res.writeHead(200, { "content-length": bytes.length });
        bytesSent += bytes.length;
        res.end(bytes);
      }, 300);
      return;
    }
    const match = /^bytes=(\d*)-(\d+)$/.exec(range);
    if (!match) {
      res.writeHead(416).end();
      return;
    }
    const start = match[1] ? Number(match[1]) : Math.max(0, bytes.length - Number(match[2]));
    const end = match[1] ? Number(match[2]) : bytes.length - 1;
    const short = mode === "short" && start === archive.headerEnd;
    const body = bytes.subarray(start, end + (short ? 0 : 1));
    res.writeHead(206, {
      "content-range": `bytes ${start}-${end}/${bytes.length}`,
      ...(short ? { "transfer-encoding": "chunked" } : { "content-length": body.length }),
    });
    bytesSent += body.length;
    res.end(body);
  });
  await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
  const { port } = server.address() as AddressInfo;
  archiveUrl = `http://127.0.0.1:${port}/runtime.nupkg`;
  globalThis.fetch = routePackageDownloadsTo(archiveUrl);
});

afterEach(async () => {
  globalThis.fetch = realFetch;
  await new Promise<void>((done) => server.close(() => done()));
  rmSync(workDir, { recursive: true, force: true });
});

function seams(platformInfo: typeof PLATFORM_INFO = PLATFORM_INFO) {
  return {
    platformInfo,
    systemSearchPaths: [],
    lockPollMs: 50,
  };
}

function expectInstalled(storageDir: string, result: string | null): void {
  const ortDir = join(storageDir, "onnxruntime", ORT_VERSION);
  expect(result).toBe(ortDir);
  expect(readFileSync(join(ortDir, LIB_NAME), "utf8")).toBe(FAKE_LIBRARY);
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
      return url === ${JSON.stringify(PACKAGE_URL)}
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
  test("publishes the runtime from a server that ignores Range", async () => {
    const storageDir = join(workDir, "storage-single");
    const result = await resolveOnnxRuntime(storageDir, seams());

    expectInstalled(storageDir, result);
    expect(requests).toHaveLength(1);
    expect(requests[0]).toMatch(/^bytes=-\d+$/);
    expect(bytesSent).toBe(archive.bytes.length);
  });

  test("two installs in one process both get the runtime from a single download", async () => {
    const storageDir = join(workDir, "storage");
    const [first, second] = await Promise.all([
      resolveOnnxRuntime(storageDir, seams()),
      resolveOnnxRuntime(storageDir, seams()),
    ]);

    expectInstalled(storageDir, first);
    expectInstalled(storageDir, second);
    expect(requests).toHaveLength(1);
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
    expect(requests).toHaveLength(1);
  }, 30_000);

  test("a failed install reports why", async () => {
    const storageDir = join(workDir, "storage-failing");
    // Port 1 has no listener, so the download itself fails.
    globalThis.fetch = routePackageDownloadsTo("http://127.0.0.1:1/runtime.nupkg");
    const result = await resolveOnnxRuntime(storageDir, seams());

    expect(result).toBeNull();
    expect(getOnnxRuntimeInstallFailure()).toContain("ONNX Runtime download failed");
    expect(existsSync(join(storageDir, "onnxruntime", ORT_VERSION))).toBe(false);
  });

  test("a download whose library is not the pinned build is never installed", async () => {
    mode = "range";
    const storageDir = join(workDir, "storage-tampered");
    const pinnedElsewhere = { ...PLATFORM_INFO, librarySha256: "0".repeat(64) };
    const result = await resolveOnnxRuntime(storageDir, seams(pinnedElsewhere));

    expect(result).toBeNull();
    expect(getOnnxRuntimeInstallFailure()).toContain("onnx runtime library hash mismatch at");
    expect(existsSync(join(storageDir, "onnxruntime", ORT_VERSION))).toBe(false);
    // The staging copy is removed too, so nothing unverified stays on disk.
    expect(readdirSync(join(storageDir, "onnxruntime"))).toEqual([]);
  });
});

describe("ONNX Runtime NuGet byte ranges", () => {
  test("downloads only the deflated native entry and ZIP metadata with Range", async () => {
    mode = "range";
    const storageDir = join(workDir, "storage-deflated");
    expectInstalled(storageDir, await resolveOnnxRuntime(storageDir, seams()));
    expect(requests).toHaveLength(4);
    expect(requests[0]).toMatch(/^bytes=-\d+$/);
    expect(requests.slice(1)).toEqual([
      `bytes=${archive.centralStart}-${archive.centralEnd - 1}`,
      `bytes=0-${archive.headerEnd - 1}`,
      `bytes=${archive.headerEnd}-${archive.libraryEnd - 1}`,
    ]);
    expect(bytesSent).toBeLessThan(archive.bytes.length);
  });

  test("publishes a stored native entry read with Range", async () => {
    mode = "range";
    archive = buildFakeArchive(0);
    const storageDir = join(workDir, "storage-stored");
    expectInstalled(storageDir, await resolveOnnxRuntime(storageDir, seams()));
    expect(requests).toHaveLength(4);
  });

  test("rejects a short Range response without publishing a runtime", async () => {
    mode = "short";
    const storageDir = join(workDir, "storage-short");
    expect(await resolveOnnxRuntime(storageDir, seams())).toBeNull();
    expect(getOnnxRuntimeInstallFailure()).toContain("short download body");
    expect(readdirSync(join(storageDir, "onnxruntime"))).toEqual([]);
  });

  test("rejects an oversized full-package response before downloading its body", async () => {
    mode = "oversized";
    const storageDir = join(workDir, "storage-oversized");
    expect(await resolveOnnxRuntime(storageDir, seams())).toBeNull();
    expect(getOnnxRuntimeInstallFailure()).toContain("download exceeds 209715200 bytes");
    expect(readdirSync(join(storageDir, "onnxruntime"))).toEqual([]);
  });

  test("rejects encrypted entries before reading their payload", async () => {
    mode = "range";
    archive = buildFakeArchive(8, 1);
    const storageDir = join(workDir, "storage-encrypted");
    expect(await resolveOnnxRuntime(storageDir, seams())).toBeNull();
    expect(getOnnxRuntimeInstallFailure()).toContain("encrypted ZIP entries");
    expect(requests).toHaveLength(2);
    expect(readdirSync(join(storageDir, "onnxruntime"))).toEqual([]);
  });

  test("rejects a ZIP64 central directory marker", async () => {
    mode = "range";
    archive.bytes.writeUInt32LE(0xffffffff, archive.centralEnd + 16);
    const storageDir = join(workDir, "storage-zip64");
    expect(await resolveOnnxRuntime(storageDir, seams())).toBeNull();
    expect(getOnnxRuntimeInstallFailure()).toContain("ZIP64 packages");
    expect(requests).toHaveLength(1);
    expect(readdirSync(join(storageDir, "onnxruntime"))).toEqual([]);
  });
});
