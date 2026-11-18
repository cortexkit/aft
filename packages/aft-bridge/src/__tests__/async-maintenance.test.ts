import { expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __test__ } from "../onnx-runtime.js";
import { execTarExtraction, windowsTarExecutable } from "../tar-executable.js";

test("ONNX zip extraction yields while the extractor owns the work", async () => {
  const dir = mkdtempSync(join(tmpdir(), "aft-zip-async-"));
  try {
    const file = join(dir, "library.txt");
    const archive = join(dir, "ort.zip");
    // A stored ZIP containing library.txt, independent of a zip writer on PATH.
    writeFileSync(
      archive,
      Buffer.from(
        "UEsDBBQAAAAAAOcIS131Lkh4DQAAAA0AAAALAAAAbGlicmFyeS50eHRsaWJyYXJ5IGJ5dGVzUEsBAhQDFAAAAAAA5whLXfUuSHgNAAAADQAAAAsAAAAAAAAAAAAAAIABAAAAAGxpYnJhcnkudHh0UEsFBgAAAAABAAEAOQAAADYAAAAAAA==",
        "base64",
      ),
    );
    const extraction = __test__.extractZipArchive(archive, dir);
    let settled = false;
    void extraction.then(() => {
      settled = true;
    });
    for (let i = 0; i < 10; i++) await Promise.resolve();
    expect(settled).toBe(false);
    await extraction;
    expect(readFileSync(file, "utf8")).toBe("library bytes");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("private ONNX tar extraction yields and preserves private member permissions", async () => {
  const dir = mkdtempSync(join(tmpdir(), "aft-tar-async-"));
  try {
    const file = join(dir, "library.txt");
    const archive = join(dir, "ort.tar.gz");
    writeFileSync(file, "library bytes", { mode: 0o644 });
    execFileSync(windowsTarExecutable(), ["czf", archive, "-C", dir, "library.txt"]);
    rmSync(file);
    const extraction = execTarExtraction(["xzf", archive, "-C", dir], 120_000, true);
    let settled = false;
    void extraction.then(() => {
      settled = true;
    });
    for (let i = 0; i < 10; i++) await Promise.resolve();
    expect(settled).toBe(false);
    await extraction;
    expect(readFileSync(file, "utf8")).toBe("library bytes");
    if (process.platform !== "win32") expect(statSync(file).mode & 0o077).toBe(0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
